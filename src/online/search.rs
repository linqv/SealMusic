//! Catalog search adapters for the 5 LX online platforms (`kw`, `kg`, `tx`, `wy`, `mg`).
//! Each adapter produces `OnlineTrack` metadata compatible with LX custom source scripts.
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use lx_probe::crypto::{AesMode, aes_encrypt, md5_hex, sha1};
use reqwest::Response;
use serde_json::{Value, json};

use super::{API_BODY_LIMIT, OnlineTrack, read_bounded};
use crate::library::Track;

const PAGE_SIZE: u32 = 30;
const KW_SEARCH_URL: &str = concat!(
    "https://search.kuwo.cn/r.s?client=kt&ver=kwplayer_ar_9.2.2.1&vipver=1",
    "&uid=794762570&show_copyright_off=1&newver=1&ft=music&cluster=0&strategy=2012",
    "&encoding=utf8&rformat=json&vermerge=1&mobi=1&issubtitle=1"
);
const KG_SEARCH_URL: &str = "http://songsearch.kugou.com/song_search_v2";
const TX_SEARCH_URL: &str = "https://u.y.qq.com/cgi-bin/musics.fcg";
const WY_EAPI_URL: &str = "http://interface.music.163.com/eapi/batch";
const WY_SEARCH_PATH: &str = "/api/search/song/list/page";
const MG_SEARCH_URL: &str = "https://jadeite.migu.cn/music_search/v3/search/searchAll";
pub(super) const DESKTOP_UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/60.0.3112.90 Safari/537.36";

pub struct SearchPage {
    pub tracks: Vec<Arc<Track>>,
    pub has_next: bool,
    pub partial_failure: bool,
}

pub const DEFAULT_SEARCH_SOURCE: &str = "kw";

pub fn is_search_source(source: &str) -> bool {
    matches!(source, "kw" | "kg" | "tx" | "wy" | "mg" | "all")
}

pub async fn search(source: &str, query: &str, page: u32) -> Result<SearchPage, ()> {
    if page == 0 {
        return Err(());
    }
    if source == "all" {
        let (kw, kg, tx, wy, mg) = tokio::join!(
            search_platform("kw", query, page),
            search_platform("kg", query, page),
            search_platform("tx", query, page),
            search_platform("wy", query, page),
            search_platform("mg", query, page),
        );
        return merge_platform_pages([kw, kg, tx, wy, mg]);
    }
    search_platform(source, query, page).await
}

fn merge_platform_pages(pages: [Result<SearchPage, ()>; 5]) -> Result<SearchPage, ()> {
    let partial_failure = pages.iter().any(Result::is_err);
    let mut has_next = false;
    let mut platform_tracks: Vec<_> = pages
        .into_iter()
        .flatten()
        .map(|page| {
            has_next |= page.has_next;
            page.tracks.into_iter()
        })
        .collect();
    if platform_tracks.is_empty() {
        return Err(());
    }

    // Interleave platform ranks so every successful platform appears near the top.
    let mut tracks = Vec::new();
    loop {
        let count = tracks.len();
        tracks.extend(platform_tracks.iter_mut().filter_map(Iterator::next));
        if tracks.len() == count {
            break;
        }
    }
    Ok(SearchPage {
        tracks,
        has_next,
        partial_failure,
    })
}

async fn search_platform(source: &str, query: &str, page: u32) -> Result<SearchPage, ()> {
    let client = super::api_client().ok_or(())?;
    let timeout = Duration::from_secs(15);
    let response = match source {
        "kw" => {
            let index = (page - 1).to_string();
            client
                .get(KW_SEARCH_URL)
                .timeout(timeout)
                .query(&[("all", query), ("pn", index.as_str()), ("rn", &PAGE_SIZE.to_string())])
                .send()
                .await
        }
        "kg" => {
            client
                .get(KG_SEARCH_URL)
                .timeout(timeout)
                .query(&[
                    ("platform", "AndroidFilter"),
                    ("iscorrection", "1"),
                    ("keyword", query),
                    ("hifiquality", "0"),
                    ("pagesize", &PAGE_SIZE.to_string()),
                    ("PrivilegeFilter", "0"),
                    ("page", &page.to_string()),
                ])
                .send()
                .await
        }
        "tx" => {
            let body = tx_request_body(query, page, &tx_search_id())?;
            let sign = zzc_sign(&body)?;
            client
                .post(TX_SEARCH_URL)
                .timeout(timeout)
                .query(&[("sign", sign.as_str())])
                .header("User-Agent", "QQMusic 14090508(android 12)")
                .header("Content-Type", "application/json")
                .body(body)
                .send()
                .await
        }
        "wy" => {
            let params = wy_eapi(
                WY_SEARCH_PATH,
                &json!({
                    "keyword": query,
                    "needCorrect": "1",
                    "channel": "typing",
                    "offset": u64::from(PAGE_SIZE) * u64::from(page - 1),
                    "scene": "normal",
                    "total": true,
                    "limit": PAGE_SIZE,
                }),
            )?;
            client
                .post(WY_EAPI_URL)
                .timeout(timeout)
                .header("User-Agent", DESKTOP_UA)
                .header("origin", "https://music.163.com")
                .header("Content-Type", "application/x-www-form-urlencoded")
                .body(format!("params={params}"))
                .send()
                .await
        }
        "mg" => {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .to_string();
            let (device_id, sign) = mg_sign(&time, query)?;
            client
                .get(MG_SEARCH_URL)
                .timeout(timeout)
                .query(&[
                    ("isCorrect", "0"),
                    ("isCopyright", "1"),
                    (
                        "searchSwitch",
                        r#"{"song":1,"album":0,"singer":0,"tagSong":1,"mvSong":0,"bestShow":1,"songlist":0,"lyricSong":0}"#,
                    ),
                    ("pageSize", &PAGE_SIZE.to_string()),
                    ("text", query),
                    ("pageNo", &page.to_string()),
                    ("sort", "0"),
                    ("sid", "USS"),
                ])
                .header("uiVersion", "A_music_3.6.1")
                .header("deviceId", device_id)
                .header("timestamp", &time)
                .header("sign", &sign)
                .header("channel", "0146921")
                .header(
                    "User-Agent",
                    "Mozilla/5.0 (Linux; U; Android 11.0.0; zh-cn; MI 11 Build/OPR1.170623.032) AppleWebKit/534.30 (KHTML, like Gecko) Version/4.0 Mobile Safari/534.30",
                )
                .send()
                .await
        }
        _ => return Err(()),
    };
    let response = response.and_then(Response::error_for_status).map_err(|err| {
        log::warn!("Catalog search request failed ({source}): {}", err.without_url());
    })?;
    let bytes = read_bounded(response, API_BODY_LIMIT).await.ok_or(())?;
    let body = serde_json::from_slice(&bytes).map_err(|_| ())?;
    parse_source_page(source, body, page)
}

fn parse_source_page(source: &str, body: Value, page: u32) -> Result<SearchPage, ()> {
    match source {
        "kw" => parse_kw(body, page),
        "kg" => parse_kg(body, page),
        "tx" => parse_tx(body, page),
        "wy" => parse_wy(body, page),
        "mg" => parse_mg(body, page),
        _ => Err(()),
    }
}

pub(super) fn number(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.trim().parse().ok())
}

fn name(value: &Value) -> String {
    super::decode_html(value.as_str().unwrap_or_default())
}

fn format_singer(singers: &Value) -> String {
    if let Some(arr) = singers.as_array() {
        let joined = arr
            .iter()
            .filter_map(|item| item["name"].as_str().or_else(|| item.as_str()))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("、");
        super::decode_html(&joined)
    } else {
        name(singers)
    }
}

fn format_interval(secs: u64) -> String {
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn format_size(size: u64) -> String {
    if size == 0 {
        return "0 B".into();
    }
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut unit = 0;
    let mut val = size as f64;
    while val >= 1024.0 && unit + 1 < UNITS.len() {
        val /= 1024.0;
        unit += 1;
    }
    format!("{val:.2} {}", UNITS[unit])
}

fn collect_tracks(
    rows: Option<&Vec<Value>>,
    total: u64,
    page: u32,
    mut map_row: impl FnMut(&Value, &mut Vec<Arc<Track>>),
) -> Result<SearchPage, ()> {
    let mut raw = Vec::new();
    for row in rows.into_iter().flatten().take(PAGE_SIZE as usize) {
        map_row(row, &mut raw);
    }
    let mut seen = HashSet::new();
    let tracks: Vec<_> = raw.into_iter().filter(|track| seen.insert(track.id.clone())).collect();
    if tracks.is_empty() && rows.is_some_and(|r| !r.is_empty()) {
        return Err(());
    }
    Ok(SearchPage {
        tracks,
        has_next: u64::from(page) * u64::from(PAGE_SIZE) < total,
        partial_failure: false,
    })
}

fn parse_kw(body: Value, page: u32) -> Result<SearchPage, ()> {
    let total = number(&body["TOTAL"]).ok_or(())?;
    let rows = body["abslist"].as_array();
    if total > 0 && (rows.is_none() || number(&body["SHOW"]) == Some(0)) {
        return Err(());
    }
    collect_tracks(rows, total, page, |row, out| {
        if let Some(track) = kw_track(row) {
            out.push(track);
        }
    })
}

fn kw_track(row: &Value) -> Option<Arc<Track>> {
    let id = row["MUSICRID"].as_str()?.strip_prefix("MUSIC_")?;
    let secs = number(&row["DURATION"]).unwrap_or_default();
    let types: Vec<_> = row["N_MINFO"]
        .as_str()
        .unwrap_or_default()
        .split(';')
        .filter_map(|entry| {
            let field = |key: &str| entry.split(',').find_map(|s| s.trim().strip_prefix(key));
            let quality = match field("bitrate:")? {
                "128" => "128k",
                "320" => "320k",
                "2000" => "flac",
                "4000" => "flac24bit",
                _ => return None,
            };
            Some(json!({"type":quality, "size":field("size:").unwrap_or_default().to_uppercase()}))
        })
        .collect();
    let img = kw_row_img(row);
    OnlineTrack::from_info(json!({
        "source":"kw", "songmid":id, "name":name(&row["SONGNAME"]),
        "singer":name(&row["ARTIST"]).replace('&', "、"), "albumName":name(&row["ALBUM"]),
        "albumId":super::id_string(&row["ALBUMID"]).unwrap_or_default(),
        "interval":format_interval(secs),
        "types":types, "img":img, "lrc":null, "otherSource":null, "typeUrl":{}
    }))
    .ok()?
    .track()
    .ok()
}

fn kw_row_img(row: &Value) -> Value {
    if let Some(pic) = row["hts_MVPIC"].as_str().map(str::trim).filter(|s| !s.is_empty())
        && (pic.starts_with("http://") || pic.starts_with("https://"))
    {
        return Value::String(pic.to_owned());
    }
    if let Some(short) = row["web_albumpic_short"].as_str().map(str::trim).filter(|s| !s.is_empty())
    {
        if short.starts_with("http://") || short.starts_with("https://") {
            return Value::String(short.to_owned());
        }
        let path = short
            .strip_prefix("120/")
            .map(|rest| format!("500/{rest}"))
            .unwrap_or_else(|| short.trim_start_matches('/').to_owned());
        return Value::String(format!("https://img4.kuwo.cn/star/albumcover/{path}"));
    }
    Value::Null
}

fn parse_kg(body: Value, page: u32) -> Result<SearchPage, ()> {
    if number(&body["error_code"]) != Some(0) {
        return Err(());
    }
    let data = &body["data"];
    let total = number(&data["total"]).ok_or(())?;
    let rows = data["lists"].as_array();
    if total > 0 && rows.is_none() {
        return Err(());
    }
    collect_tracks(rows, total, page, |row, out| {
        if let Some(track) = kg_track(row) {
            out.push(track);
        }
        if let Some(grp) = row["Grp"].as_array() {
            out.extend(grp.iter().filter_map(kg_track));
        }
    })
}

fn kg_track(item: &Value) -> Option<Arc<Track>> {
    let file_hash = item["FileHash"].as_str()?.trim();
    if file_hash.is_empty() || super::id_string(&item["Audioid"]).is_none() {
        return None;
    }
    let mut types = Vec::new();
    for (size_key, hash_key, quality) in [
        ("FileSize", "FileHash", "128k"),
        ("HQFileSize", "HQFileHash", "320k"),
        ("SQFileSize", "SQFileHash", "flac"),
        ("ResFileSize", "ResFileHash", "flac24bit"),
    ] {
        if let Some(size) = number(&item[size_key]).filter(|&n| n > 0)
            && let Some(hash) = item[hash_key].as_str().map(str::trim).filter(|s| !s.is_empty())
        {
            types.push(json!({"type": quality, "size": format_size(size), "hash": hash}));
        }
    }
    let base_name = item["OriSongName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| item["SongName"].as_str())
        .unwrap_or_default();
    let suffix = item["Suffix"].as_str().unwrap_or_default().trim();
    let raw_title = if suffix.is_empty() {
        base_name.to_owned()
    } else {
        format!("{base_name} {suffix}")
    };
    let singer = if item["Singers"].is_array() {
        format_singer(&item["Singers"])
    } else {
        name(&item["SingerName"])
    };
    let secs = number(&item["Duration"]).unwrap_or_default();
    let img = item["Image"]
        .as_str()
        .or_else(|| item["trans_param"]["union_cover"].as_str())
        .map(str::trim)
        .filter(|s| s.starts_with("http://") || s.starts_with("https://"))
        .map(|s| Value::String(s.replace("{size}", "400")))
        .unwrap_or(Value::Null);
    OnlineTrack::from_info(json!({
        "source": "kg",
        "songmid": item["Audioid"],
        "name": super::decode_html(&raw_title),
        "singer": singer,
        "albumName": name(&item["AlbumName"]),
        "albumId": super::id_string(&item["AlbumID"]).unwrap_or_default(),
        "albumAudioId": item["MixSongID"],
        "interval": format_interval(secs),
        "_interval": secs,
        "hash": file_hash,
        "types": types,
        "img": img,
        "lrc": null,
        "otherSource": null,
        "typeUrl": {},
    }))
    .ok()?
    .track()
    .ok()
}

fn tx_search_id() -> String {
    let guid: u128 = rand::random();
    let tail: u32 = rand::random::<u32>() % 100_000;
    format!("{guid:032X}{tail:05}")
}

fn tx_request_body(query: &str, page: u32, search_id: &str) -> Result<String, ()> {
    serde_json::to_string(&json!({
        "comm": {
            "_channelid": "0",
            "_os_version": "6.2.9200-2",
            "ct": "19",
            "cv": "2151",
            "guid": "1F70E520B2EAA7D25E11760783C53CA9",
            "patch": "118",
            "psrf_access_token_expiresAt": 0,
            "psrf_qqaccess_token": "",
            "psrf_qqopenid": "",
            "psrf_qqunionid": "",
            "tmeAppID": "qqmusic",
            "tmeLoginType": 0,
            "uin": "0",
            "wid": "7223299733393904640",
        },
        "music.search.SearchCgiService": {
            "module": "music.search.SearchCgiService",
            "method": "DoSearchForQQMusicDesktop",
            "param": {
                "grp": 1,
                "num_per_page": PAGE_SIZE,
                "page_num": page,
                "query": query,
                "remoteplace": "txt.newclient.top",
                "search_type": 0,
                "searchid": search_id,
            },
        },
    }))
    .map_err(|_| ())
}

fn zzc_sign(text: &str) -> Result<String, ()> {
    const PART_1_INDEXES: [usize; 8] = [23, 14, 6, 36, 16, 40, 7, 19];
    const PART_2_INDEXES: [usize; 8] = [16, 1, 32, 12, 19, 27, 8, 5];
    const SCRAMBLE_VALUES: [u8; 20] = [
        89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161, 121,
        179,
    ];
    let digest = sha1(text.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let pick = |indexes: &[usize]| -> String {
        indexes.iter().filter_map(|&idx| hex.as_bytes().get(idx).map(|&b| b as char)).collect()
    };
    let part1 = pick(&PART_1_INDEXES);
    let part2 = pick(&PART_2_INDEXES);
    let part3: Vec<u8> = SCRAMBLE_VALUES.iter().zip(digest.iter()).map(|(&a, &b)| a ^ b).collect();
    let b64: String =
        BASE64.encode(part3).chars().filter(|c| !matches!(c, '/' | '+' | '=')).collect();
    Ok(format!("zzc{part1}{b64}{part2}").to_ascii_lowercase())
}

fn parse_tx(body: Value, page: u32) -> Result<SearchPage, ()> {
    if number(&body["code"]) != Some(0) {
        return Err(());
    }
    let req = body.get("music.search.SearchCgiService").or_else(|| body.get("req")).ok_or(())?;
    if number(&req["code"]) != Some(0) {
        return Err(());
    }
    let data = &req["data"];
    let total = number(&data["meta"]["sum"]).ok_or(())?;
    let rows = data["body"]["song"]["list"].as_array();
    if total > 0 && rows.is_none() {
        return Err(());
    }
    collect_tracks(rows, total, page, |row, out| {
        if let Some(track) = tx_track(row) {
            out.push(track);
        }
    })
}

fn tx_track(item: &Value) -> Option<Arc<Track>> {
    let file = &item["file"];
    let media_mid = file["media_mid"].as_str()?.trim();
    let songmid = item["mid"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| item["songmid"].as_str().map(str::trim).filter(|s| !s.is_empty()))?;
    if media_mid.is_empty() {
        return None;
    }
    let mut types = Vec::new();
    for (key, quality) in [
        ("size_128mp3", "128k"),
        ("size_320mp3", "320k"),
        ("size_flac", "flac"),
        ("size_hires", "flac24bit"),
    ] {
        if let Some(size) = number(&file[key]).filter(|&n| n > 0) {
            types.push(json!({"type": quality, "size": format_size(size)}));
        }
    }
    let album_mid = item["album"]["mid"].as_str().unwrap_or_default().trim();
    let singer_mid = item["singer"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|s| s["mid"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let img = if !album_mid.is_empty() && album_mid != "空" {
        Value::String(format!("https://y.gtimg.cn/music/photo_new/T002R500x500M000{album_mid}.jpg"))
    } else if let Some(mid) = singer_mid {
        Value::String(format!("https://y.gtimg.cn/music/photo_new/T001R500x500M000{mid}.jpg"))
    } else {
        Value::Null
    };
    let title = item["title"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| item["name"].as_str())
        .map(super::decode_html)
        .unwrap_or_default();
    let secs = number(&item["interval"]).unwrap_or_default();
    OnlineTrack::from_info(json!({
        "source": "tx",
        "songmid": songmid,
        "strMediaMid": media_mid,
        "albumMid": album_mid,
        "albumId": album_mid,
        "albumName": name(&item["album"]["name"]),
        "name": title,
        "singer": format_singer(&item["singer"]),
        "songId": item["id"],
        "interval": format_interval(secs),
        "img": img,
        "lrc": null,
        "otherSource": null,
        "types": types,
        "typeUrl": {},
    }))
    .ok()?
    .track()
    .ok()
}

pub(super) fn wy_eapi(url: &str, object: &Value) -> Result<String, ()> {
    const EAPI_KEY: &[u8; 16] = b"e82ckenh8dichen8";
    let text = serde_json::to_string(object).map_err(|_| ())?;
    let message = format!("nobody{url}use{text}md5forencrypt");
    let digest = md5_hex(message.as_bytes());
    let data = format!("{url}-36cd479b6b5-{text}-36cd479b6b5-{digest}");
    let encrypted =
        aes_encrypt(AesMode::Aes128Ecb, EAPI_KEY, &[], data.as_bytes()).map_err(|_| ())?;
    Ok(encrypted.iter().map(|b| format!("{b:02X}")).collect())
}

fn parse_wy(body: Value, page: u32) -> Result<SearchPage, ()> {
    if number(&body["code"]) != Some(200) {
        return Err(());
    }
    let data = &body["data"];
    let total = number(&data["totalCount"]).unwrap_or_default();
    let rows = data["resources"].as_array();
    if total > 0 && rows.is_none() {
        return Err(());
    }
    collect_tracks(rows, total, page, |row, out| {
        if let Some(track) = wy_track(row) {
            out.push(track);
        }
    })
}

fn wy_track(entry: &Value) -> Option<Arc<Track>> {
    let item = entry.get("baseInfo").and_then(|b| b.get("simpleSongData")).unwrap_or(entry);
    let privilege = &item["privilege"];
    let maxbr = number(&privilege["maxbr"]).unwrap_or_default();
    let size_of = |key: &str| number(&item[key]["size"]).filter(|&n| n > 0).map(format_size);
    let mut types = Vec::new();
    if maxbr >= 128_000 {
        types.push(json!({"type": "128k", "size": size_of("l")}));
    }
    if maxbr >= 320_000 {
        types.push(json!({"type": "320k", "size": size_of("h")}));
    }
    if maxbr >= 999_000 {
        types.push(json!({"type": "flac", "size": size_of("sq")}));
    }
    if privilege["maxBrLevel"].as_str() == Some("hires") {
        types.push(json!({"type": "flac24bit", "size": size_of("hr")}));
    }
    let secs = number(&item["dt"]).unwrap_or_default() / 1000;
    OnlineTrack::from_info(json!({
        "source": "wy",
        "songmid": item["id"],
        "name": name(&item["name"]),
        "singer": format_singer(&item["ar"]),
        "albumName": name(&item["al"]["name"]),
        "albumId": super::id_string(&item["al"]["id"]).unwrap_or_default(),
        "interval": format_interval(secs),
        "img": item["al"]["picUrl"],
        "lrc": null,
        "otherSource": null,
        "types": types,
        "typeUrl": {},
    }))
    .ok()?
    .track()
    .ok()
}

fn mg_sign(time: &str, query: &str) -> Result<(&'static str, String), ()> {
    const DEVICE_ID: &str = "963B7AA0D21511ED807EE5846EC87D20";
    const SIG_MD5: &str = "6cdc72a439cef99a3418d2a78aa28c73";
    let raw = format!("{query}{SIG_MD5}yyapp2d16148780a1dcc7408e06336b98cfd50{DEVICE_ID}{time}");
    Ok((DEVICE_ID, md5_hex(raw.as_bytes())))
}

fn parse_mg(body: Value, page: u32) -> Result<SearchPage, ()> {
    if body["code"].as_str() != Some("000000") {
        return Err(());
    }
    let data = &body["songResultData"];
    if data.is_null() {
        return Ok(SearchPage {
            tracks: vec![],
            has_next: false,
            partial_failure: false,
        });
    }
    let total = number(&data["totalCount"]).unwrap_or_default();
    let rows = data["resultList"].as_array();
    if total > 0 && rows.is_none() {
        return Err(());
    }
    collect_tracks(rows, total, page, |group, out| {
        if let Some(items) = group.as_array() {
            out.extend(items.iter().filter_map(mg_track));
        } else if let Some(track) = mg_track(group) {
            out.push(track);
        }
    })
}

fn mg_track(data: &Value) -> Option<Arc<Track>> {
    let song_id = super::id_string(&data["songId"])?;
    let copyright_id = data["copyrightId"].as_str()?.trim();
    if copyright_id.is_empty() {
        return None;
    }
    let mut types = Vec::new();
    if let Some(formats) = data["audioFormats"].as_array() {
        for entry in formats {
            let quality = match entry["formatType"].as_str() {
                Some("PQ") => "128k",
                Some("HQ") => "320k",
                Some("SQ") => "flac",
                Some("ZQ24") => "flac24bit",
                _ => continue,
            };
            let size = number(&entry["asize"])
                .or_else(|| number(&entry["isize"]))
                .filter(|&n| n > 0)
                .map(format_size);
            types.push(json!({"type": quality, "size": size}));
        }
    }
    let secs = number(&data["duration"]).unwrap_or_default();
    let img = mg_row_img(data);
    OnlineTrack::from_info(json!({
        "source": "mg",
        "songmid": song_id,
        "copyrightId": copyright_id,
        "name": name(&data["name"]),
        "singer": format_singer(&data["singerList"]),
        "albumName": name(&data["album"]),
        "albumId": super::id_string(&data["albumId"]).unwrap_or_default(),
        "interval": format_interval(secs),
        "img": img,
        "lrc": null,
        "lrcUrl": data["lrcUrl"],
        "mrcUrl": data["mrcurl"],
        "trcUrl": data["trcUrl"],
        "otherSource": null,
        "types": types,
        "typeUrl": {},
    }))
    .ok()?
    .track()
    .ok()
}

pub(super) fn normalize_mg_img(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(trimmed.to_owned());
    }
    if trimmed.starts_with("//") {
        return Some(format!("https:{trimmed}"));
    }
    if trimmed.starts_with('/') {
        Some(format!("http://d.musicapp.migu.cn{trimmed}"))
    } else {
        Some(format!("http://d.musicapp.migu.cn/{trimmed}"))
    }
}

fn mg_row_img(data: &Value) -> Value {
    for key in ["img3", "img2", "img1"] {
        if let Some(url) = data[key].as_str().and_then(normalize_mg_img) {
            return Value::String(url);
        }
    }
    for list_key in ["imgItems", "albumImgs"] {
        if let Some(items) = data[list_key].as_array() {
            for preferred in ["03", "02", "01"] {
                if let Some(url) = items
                    .iter()
                    .find(|item| item["imgSizeType"].as_str() == Some(preferred))
                    .and_then(|item| item["img"].as_str().or_else(|| item["imgUrl"].as_str()))
                    .and_then(normalize_mg_img)
                {
                    return Value::String(url);
                }
            }
            if let Some(url) = items
                .iter()
                .rev()
                .filter_map(|item| item["img"].as_str().or_else(|| item["imgUrl"].as_str()))
                .find_map(normalize_mg_img)
            {
                return Value::String(url);
            }
        }
    }
    Value::Null
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::TrackOrigin;

    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../tools/lx-probe/fixtures/tracks.json")).unwrap()
    }

    #[test]
    fn aggregate_interleaves_results_and_keeps_successful_pages_on_failure() {
        let fixtures = fixtures();
        let track = |index: usize| {
            OnlineTrack::from_info(fixtures[index]["info"]["musicInfo"].clone())
                .unwrap()
                .track()
                .unwrap()
        };
        let first = track(0);
        let second = kw_track(&json!({"MUSICRID": "MUSIC_7000", "SONGNAME": "Second"})).unwrap();
        let another = track(2);
        let pages = [
            Ok(SearchPage {
                tracks: vec![first.clone(), second.clone()],
                has_next: false,
                partial_failure: false,
            }),
            Err(()),
            Ok(SearchPage {
                tracks: vec![another.clone()],
                has_next: true,
                partial_failure: false,
            }),
            Ok(SearchPage {
                tracks: vec![],
                has_next: false,
                partial_failure: false,
            }),
            Err(()),
        ];
        let merged = merge_platform_pages(pages).unwrap();
        let ids: Vec<_> = merged.tracks.iter().map(|track| &track.id).collect();
        assert_eq!(ids, [&first.id, &another.id, &second.id]);
        assert!(merged.has_next);
        assert!(merged.partial_failure);
        assert!(merge_platform_pages([Err(()), Err(()), Err(()), Err(()), Err(())]).is_err());
    }

    #[test]
    fn kuwo_rows_keep_lx_identity_and_per_quality_metadata() {
        let row = json!({"MUSICRID":"MUSIC_1001","SONGNAME":"A &amp; B","ARTIST":"X&Y",
            "DURATION":"75","ALBUMID":"7","ALBUM":"Album",
            "N_MINFO":"level:ff,bitrate:2000,format:flac,size:9m;level:p,bitrate:128,format:mp3,size:1m"});
        let body = json!({"SHOW":"3","TOTAL":"31","abslist":[{},row,row]});
        let page = parse_source_page("kw", body.clone(), 1).unwrap();
        assert!(page.has_next);
        assert!(!parse_source_page("kw", body, 2).unwrap().has_next);
        assert_eq!(page.tracks.len(), 1);
        let track = &page.tracks[0];
        assert_eq!(track.title, "A & B");
        assert_eq!(track.artist, "X、Y");
        assert_eq!(track.duration, Duration::from_secs(75));
        let TrackOrigin::Online(reference) = &track.origin else {
            panic!()
        };
        let info = &reference.action()["info"]["musicInfo"];
        assert_eq!(info["songmid"], "1001");
        assert_eq!(info["_types"]["flac"]["size"], "9M");
        let from_link = OnlineTrack::from_info(fixtures()[0]["info"]["musicInfo"].clone()).unwrap();
        assert_eq!(track.id, from_link.track().unwrap().id);
    }

    #[test]
    fn kugou_tx_wy_and_migu_rows_match_lx_fixtures_and_qualities() {
        let fx = fixtures();

        // 1. Kugou (kg): includes Grp child rows, case-insensitive hash identity, and per-quality hashes.
        let kg_row = json!({
            "Audioid": 1002,
            "OriSongName": "KG &amp; Song",
            "Suffix": "(Live)",
            "Singers": [{"name": "Alpha"}, {"name": "Beta"}],
            "AlbumName": "KG Album",
            "AlbumID": 1,
            "MixSongID": 99,
            "Duration": 185,
            "FileHash": "11111111111111111111111111111111",
            "FileSize": 1048576,
            "HQFileHash": "22222222222222222222222222222222",
            "HQFileSize": 2097152,
            "SQFileHash": "33333333333333333333333333333333",
            "SQFileSize": 10485760,
            "ResFileHash": "",
            "ResFileSize": 0,
            "Grp": [{
                "Audioid": 1002,
                "OriSongName": "Duplicate Hash",
                "Singers": [{"name": "Alpha"}],
                "Duration": 185,
                "FileHash": "11111111111111111111111111111111",
                "FileSize": 1048576
            }]
        });
        let kg_page = parse_source_page(
            "kg",
            json!({"error_code": 0, "data": {"total": 1, "lists": [kg_row]}}),
            1,
        )
        .unwrap();
        assert_eq!(kg_page.tracks.len(), 1);
        let kg_track = &kg_page.tracks[0];
        assert_eq!(kg_track.title, "KG & Song (Live)");
        assert_eq!(kg_track.artist, "Alpha、Beta");
        assert_eq!(kg_track.duration, Duration::from_secs(185));
        let kg_expected = OnlineTrack::from_info(fx[1]["info"]["musicInfo"].clone()).unwrap();
        assert_eq!(kg_track.id, kg_expected.track().unwrap().id);
        let TrackOrigin::Online(kg_ref) = &kg_track.origin else {
            panic!()
        };
        assert_eq!(
            kg_ref.action()["info"]["musicInfo"]["_types"]["320k"]["hash"],
            "22222222222222222222222222222222"
        );

        // 2. QQ Music (tx): requires file.media_mid and maps mid -> songmid, media_mid -> strMediaMid.
        let tx_row = json!({
            "id": 12345,
            "mid": "p0SongMid",
            "title": "TX Title",
            "singer": [{"name": "Singer A"}, {"name": "Singer B"}],
            "album": {"mid": "p0AlbumMid", "name": "TX Album"},
            "interval": 200,
            "file": {
                "media_mid": "p0MediaMid",
                "size_128mp3": 1048576,
                "size_320mp3": 3145728,
                "size_flac": 20971520,
                "size_hires": 0
            }
        });
        let tx_page = parse_source_page(
            "tx",
            json!({
                "code": 0,
                "music.search.SearchCgiService": {
                    "code": 0,
                    "data": {
                        "meta": {"sum": 45},
                        "body": {"song": {"list": [{}, tx_row]}}
                    }
                }
            }),
            1,
        )
        .unwrap();
        assert!(tx_page.has_next);
        assert_eq!(tx_page.tracks.len(), 1);
        let tx_track = &tx_page.tracks[0];
        let tx_expected = OnlineTrack::from_info(fx[2]["info"]["musicInfo"].clone()).unwrap();
        assert_eq!(tx_track.id, tx_expected.track().unwrap().id);
        let TrackOrigin::Online(tx_ref) = &tx_track.origin else {
            panic!()
        };
        assert_eq!(tx_track.album, "TX Album");
        assert_eq!(tx_ref.action()["info"]["musicInfo"]["strMediaMid"], "p0MediaMid");
        assert_eq!(tx_ref.action()["info"]["musicInfo"]["_types"]["flac"]["size"], "20.00 MiB");
        assert_eq!(
            tx_ref.action()["info"]["musicInfo"]["img"],
            "https://y.gtimg.cn/music/photo_new/T002R500x500M000p0AlbumMid.jpg"
        );

        // 3. NetEase (wy): maps maxbr fallthrough + hires into ascending types and dt (ms) -> interval.
        let wy_row = json!({
            "baseInfo": {
                "simpleSongData": {
                    "id": 29947420,
                    "name": "Fade",
                    "ar": [{"name": "Alan Walker"}],
                    "al": {"id": 1, "name": "P0", "picUrl": "https://p1.music.126.net/a.jpg"},
                    "dt": 1000,
                    "privilege": {"maxbr": 1999000, "maxBrLevel": "hires"},
                    "l": {"size": 1048576},
                    "h": {"size": 2097152},
                    "sq": {"size": 10485760},
                    "hr": {"size": 20971520}
                }
            }
        });
        let wy_page = parse_source_page(
            "wy",
            json!({"code": 200, "data": {"totalCount": 1, "resources": [wy_row]}}),
            1,
        )
        .unwrap();
        assert!(!wy_page.has_next);
        assert_eq!(wy_page.tracks.len(), 1);
        let wy_track = &wy_page.tracks[0];
        let wy_expected = OnlineTrack::from_info(fx[3]["info"]["musicInfo"].clone()).unwrap();
        assert_eq!(wy_track.id, wy_expected.track().unwrap().id);
        let TrackOrigin::Online(wy_ref) = &wy_track.origin else {
            panic!()
        };
        let wy_action = wy_ref.action();
        let wy_types = wy_action["info"]["musicInfo"]["types"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["type"].as_str())
            .collect::<Vec<_>>();
        assert_eq!(wy_types, vec!["128k", "320k", "flac", "flac24bit"]);

        // 4. Migu (mg): flattens nested resultList groups, deduplicates by copyrightId, and maps PQ/HQ/SQ/ZQ24.
        let mg_row = json!({
            "songId": "60000000001",
            "copyrightId": "60000000001",
            "name": "Migu Song",
            "singerList": [{"name": "Migu Artist"}],
            "album": "Migu Album",
            "albumId": "1",
            "duration": 95,
            "img3": "/public/product9th/product45/2022/cover.jpg",
            "audioFormats": [
                {"formatType": "PQ", "asize": 1048576},
                {"formatType": "HQ", "isize": 3145728},
                {"formatType": "SQ", "asize": 15728640},
                {"formatType": "ZQ24", "asize": 31457280}
            ]
        });
        let mg_page = parse_source_page(
            "mg",
            json!({
                "code": "000000",
                "songResultData": {
                    "totalCount": "1",
                    "resultList": [[mg_row.clone(), mg_row]]
                }
            }),
            1,
        )
        .unwrap();
        assert_eq!(mg_page.tracks.len(), 1);
        let mg_track = &mg_page.tracks[0];
        let mg_expected = OnlineTrack::from_info(fx[4]["info"]["musicInfo"].clone()).unwrap();
        assert_eq!(mg_track.id, mg_expected.track().unwrap().id);
        assert_eq!(mg_track.album, "Migu Album");
        let TrackOrigin::Online(mg_ref) = &mg_track.origin else {
            panic!()
        };
        assert_eq!(mg_ref.action()["info"]["musicInfo"]["copyrightId"], "60000000001");
        assert_eq!(
            mg_ref.action()["info"]["musicInfo"]["img"],
            "http://d.musicapp.migu.cn/public/product9th/product45/2022/cover.jpg"
        );
        assert_eq!(
            mg_ref.action()["info"]["musicInfo"]["_types"]["flac24bit"]["size"],
            "30.00 MiB"
        );
    }

    #[test]
    fn signing_and_encryption_helpers_match_protocol_vectors() {
        assert_eq!(
            zzc_sign(r#"{"comm":{"ct":"19"}}"#).unwrap(),
            "zzc790be3fkydls8wd25ty3g8tneo6yjymkea23f3fe"
        );
        let eapi = wy_eapi(WY_SEARCH_PATH, &json!({"keyword": "Fade"})).unwrap();
        assert_eq!(
            eapi,
            "74A595527B7A1647174ADDB4F261E92F180F42F921F98E9D338C60DB20AF499CEA90E95FB2FDA117A0B5D8175C2F21E58373E20335FCAEFFFC254767452001FFDF6EEAFA9293AF7C6CC8E718741466295D866F31B4216E541BBA1515A215FEE8D76CFF0E76938EACAEF61FE01DBA83EB"
        );
        let (device_id, sign) = mg_sign("1700000000000", "Fade").unwrap();
        assert_eq!(device_id, "963B7AA0D21511ED807EE5846EC87D20");
        assert_eq!(sign.len(), 32);
    }
}
