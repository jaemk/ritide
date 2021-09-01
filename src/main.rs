use async_rwlock::RwLock;
use serde::Deserialize;
use std::convert::Infallible;
use std::env;
use std::fs;
use std::io::Read;
use std::sync::Arc;
use tracing_subscriber::fmt::format::FmtSpan;
use warp::Filter;

const TT: &str = "ritide";

lazy_static::lazy_static! {
    pub static ref CONFIG: Config = Config::load();
}

fn env_or(k: &str, default: &str) -> String {
    env::var(k).unwrap_or_else(|_| default.to_string())
}

#[derive(serde_derive::Deserialize)]
pub struct Config {
    pub version: String,
    pub host: String,
    pub port: u16,
}
impl Config {
    pub fn load() -> Self {
        let version = fs::File::open("commit_hash.txt")
            .map(|mut f| {
                let mut s = String::new();
                f.read_to_string(&mut s).expect("Error reading commit_hasg");
                s
            })
            .unwrap_or_else(|_| "unknown".to_string());
        Self {
            version,
            host: env_or("HOST", "0.0.0.0"),
            port: env_or("PORT", "80").parse().expect("invalid port"),
        }
    }

    pub fn initialize(&self) {
        tracing::info!(
            target: TT,
            version = %CONFIG.version,
            host = %CONFIG.host,
            port = %CONFIG.port,
            "initialized config",
        );
    }
}

mod de_ser_date_format {
    use chrono::{DateTime, Local, TimeZone};
    use serde::{self, Deserialize, Deserializer, Serializer};

    const FORMAT: &str = "%Y-%m-%d %H:%M";

    // The signature of a serialize_with function must follow the pattern:
    //
    //    fn serialize<S>(&T, S) -> Result<S::Ok, S::Error>
    //    where
    //        S: Serializer
    //
    // although it may also be generic over the input types T.
    pub fn serialize<S>(date: &DateTime<Local>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let s = format!("{}", date.format(FORMAT));
        serializer.serialize_str(&s)
    }

    // The signature of a deserialize_with function must follow the pattern:
    //
    //    fn deserialize<'de, D>(D) -> Result<T, D::Error>
    //    where
    //        D: Deserializer<'de>
    //
    // although it may also be generic over the output types T.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<DateTime<Local>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Local
            .datetime_from_str(&s, FORMAT)
            .map_err(serde::de::Error::custom)
    }
}

fn de_ser_float_format<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<f32, D::Error> {
    Ok(match serde_json::Value::deserialize(deserializer)? {
        serde_json::Value::String(s) => s.parse().map_err(serde::de::Error::custom)?,
        serde_json::Value::Number(num) => {
            num.as_f64()
                .ok_or_else(|| serde::de::Error::custom("Invalid number"))? as f32
        }
        _ => return Err(serde::de::Error::custom("wrong type")),
    })
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
enum Type {
    H,
    L,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Prediction {
    #[serde(with = "de_ser_date_format")]
    t: chrono::DateTime<chrono::Local>,
    #[serde(deserialize_with = "de_ser_float_format")]
    v: f32,
    #[serde(rename(serialize = "type", deserialize = "type"))]
    ty: Type,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct Tides {
    predictions: Vec<Prediction>,
}

// https://github.com/jaemk/cached/issues/83
lazy_static::lazy_static! {
    static ref CACHE: Arc<async_rwlock::RwLock<Vec<(u128, Tides)>>> = Arc::new(RwLock::new(vec![(0, Tides{predictions: vec![]})]));
}

fn time_now() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

async fn get_tides() -> Result<Tides, String> {
    let max_millis: u128 = 60 * 60 * 1000;
    {
        let tides = CACHE.read().await;
        if let Some((created_ts, tides)) = tides.get(0) {
            if (time_now() - created_ts) < max_millis {
                return Ok(tides.clone());
            }
        }
    }
    let mut store = CACHE.write().await;
    let already_set = store
        .get(0)
        .map(|(created_ts, _)| (time_now() - created_ts) < max_millis)
        .unwrap_or(false);
    if already_set {
        Ok(store[0].1.clone())
    } else {
        let tides = get_fresh_tides().await?;
        store.clear();
        store.push((time_now(), tides.clone()));
        Ok(tides)
    }
}

const STATION: u32 = 8458022;

async fn get_fresh_tides() -> Result<Tides, String> {
    // WEEKAPAUG POINT, BLOCK ISLAND SOUND, RI - Station ID: 8458022
    // https://tidesandcurrents.noaa.gov/stationhome.html?id=8458022
    let edt = chrono::FixedOffset::west(4 * 60 * 60);
    let now_edt = chrono::Utc::now().with_timezone(&edt);
    let yesterday = now_edt
        .checked_add_signed(chrono::Duration::days(-1))
        .expect("error calculating yeseterday");
    let days_out = now_edt
        .checked_add_signed(chrono::Duration::days(7))
        .expect("error calculating 7 days out");
    tracing::info!(
        target: TT,
        "loading tide info for {:?} to {:?}",
        yesterday,
        days_out
    );

    let url = format!("https://api.tidesandcurrents.noaa.gov/api/prod/datagetter?product=predictions&application=NOS.COOPS.TAC.WL&begin_date={begin}&end_date={end}&datum=MLLW&station={station}&time_zone=lst_ldt&units=english&interval=hilo&format=json",
                      begin=yesterday.format("%Y%m%d").to_string(),
                    end=days_out.format("%Y%m%d").to_string(),
                    station=STATION,
    );
    let resp = reqwest::get(url).await.unwrap().text().await.unwrap();
    tracing::debug!(target: TT, "response: {:?}", resp);
    let resp: Tides = serde_json::from_str(&resp).unwrap();
    Ok(resp)
}

#[tokio::main]
async fn main() {
    dotenv::dotenv().ok();
    // Filter traces based on the LOG env var, default all to info
    let filter = std::env::var("LOG")
        .unwrap_or_else(|_| format!("tracing=info,warp=info,{tt}=info", tt = TT));

    // Configure the default `tracing` subscriber.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_span_events(FmtSpan::CLOSE)
        .init();

    CONFIG.initialize();

    // status
    let status = warp::any().and(warp::path("status")).map(|| {
        tracing::info!(target: TT, "checking server status");
        warp::reply::json(&serde_json::json!({
            "ok": "ok",
            "version": &CONFIG.version,
        }))
    });

    // tides
    async fn req_tides() -> Result<impl warp::Reply, Infallible> {
        match get_tides().await {
            Err(e) => {
                tracing::error!(target: TT, "error getting tides {:?}", e);
                Ok(warp::reply::json(&serde_json::json!({"error": "error"})))
            }
            Ok(tides) => Ok(warp::reply::json(&serde_json::json!({ "tides": tides }))),
        }
    }
    let tides = warp::any().and(warp::path("tides")).and_then(req_tides);

    let te = Arc::new(
        tera::Tera::new("templates/**/*.html").expect("unable to compile tera termplates"),
    );

    async fn req_index(te: Arc<tera::Tera>) -> Result<impl warp::Reply, Infallible> {
        match get_tides().await {
            Err(e) => {
                tracing::error!(target: TT, "error getting tides {:?}", e);
                Ok(warp::reply::html("something went wrong".to_string()))
            }
            Ok(tides) => {
                #[derive(serde::Serialize)]
                struct Formatted {
                    time: String,
                    only_time: String,
                    level: String,
                    height: String,
                    is_next: bool,
                }
                let mut formatted = vec![];
                let mut index_of_next = 0;

                let edt = chrono::FixedOffset::west(4 * 60 * 60);
                let now_edt = chrono::Utc::now().with_timezone(&edt);
                let now_edt = now_edt.naive_local();

                for (i, t) in tides.predictions.iter().enumerate() {
                    if t.t.naive_local() > now_edt
                        && now_edt
                            .signed_duration_since(tides.predictions[index_of_next].t.naive_local())
                            .num_seconds()
                            .abs()
                            > now_edt
                                .signed_duration_since(t.t.naive_local())
                                .num_seconds()
                                .abs()
                    {
                        index_of_next = i;
                    }

                    formatted.push(Formatted {
                        time: t.t.format("%Y-%m-%d %l:%M%P").to_string(),
                        only_time: t.t.format("%l:%M%P").to_string(),
                        level: match t.ty {
                            Type::H => "High",
                            Type::L => "Low",
                        }
                        .to_string(),
                        height: ((t.v * 100.0).round() / 100.0).to_string(),
                        is_next: false,
                    })
                }
                formatted[index_of_next].is_next = true;
                let movement = if formatted[index_of_next].level == "High" {
                    "is rising"
                } else {
                    "is falling"
                };
                let dur_til_next = tides.predictions[index_of_next]
                    .t
                    .naive_local()
                    .signed_duration_since(now_edt)
                    .num_seconds();
                let hours_til_next = dur_til_next / (60 * 60);
                let minutes_til_next = (dur_til_next - (60 * 60 * hours_til_next)) / 60;
                let time_til_next = format!("{}hr {}m", hours_til_next, minutes_til_next);

                let mut ctx = tera::Context::new();
                ctx.insert("tides", &formatted);
                ctx.insert("movement", &movement);
                ctx.insert("station", &STATION);
                ctx.insert("time_til_next", &time_til_next);
                ctx.insert(
                    "next_tide_type",
                    &formatted[index_of_next].level.to_lowercase(),
                );
                ctx.insert("next_tide", &formatted[index_of_next]);
                ctx.insert("current_tide", &formatted[index_of_next - 1]);
                ctx.insert("now", &now_edt.format("%l:%M%P").to_string());
                let s = te.render("home.html", &ctx).unwrap();
                Ok(warp::reply::html(s))
            }
        }
    }

    let index = warp::any()
        .and(warp::path::end())
        .map(move || te.clone())
        .and_then(req_index);

    let static_files = warp::path("static").and(warp::fs::dir("static"));

    let routes = status
        .or(tides)
        .or(index)
        .or(static_files)
        .with(warp::filters::log::log(TT));

    let host_str = format!("{}:{}", CONFIG.host, CONFIG.port);
    let host = host_str
        .parse::<std::net::SocketAddr>()
        .map_err(|e| format!("invalid host: {}, {:?}", host_str, e))
        .unwrap();

    warp::serve(routes).run(host).await;
}
