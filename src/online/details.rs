//! Online track cover art and lyrics fetching across the 5 LX platforms
//! (`kw`, `kg`, `tx`, `wy`, `mg`), adapted from LX Music Desktop / lx-mygomusic.
use std::io::Read;

use base64::{
    Engine as _, alphabet,
    engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig},
};
use flate2::read::ZlibDecoder;
use reqwest::{Client, RequestBuilder, Response};
use serde_json::{Value, json};
use url::Url;

use super::{
    OnlineTrack, decode_html, id_string,
    search::{DESKTOP_UA, normalize_mg_img, number, wy_eapi},
};
use crate::library::{
    lyrics::{LyricLine, parse_lyrics},
    metadata::{self, TrackDetails},
};

const COVER_LIMIT: usize = 4 * 1024 * 1024;
const API_LIMIT: usize = 2 * 1024 * 1024;
const ZLIB_LIMIT: usize = 4 * 1024 * 1024;
const KW_XOR_KEY: &[u8] = b"yeelion";
const KG_KRC_KEY: [u8; 16] = [
    0x40, 0x47, 0x61, 0x77, 0x5e, 0x32, 0x74, 0x47, 0x51, 0x36, 0x31, 0x2d, 0xce, 0xd2, 0x6e, 0x69,
];
const KG_UA: &str = "KuGou2012-9020-ExpandSearchManager";
const TX_UA: &str = "Mozilla/5.0 (Windows NT 10.0; WOW64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/86.0.4240.198 Safari/537.36";
const MG_UA: &str = "Mozilla/5.0 (Linux; Android 5.1.1; Nexus 6 Build/LYZ28E) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/59.0.3071.115 Mobile Safari/537.36";
const BASE64_LENIENT: GeneralPurpose = GeneralPurpose::new(
    &alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

struct Endpoints {
    kw_pic: String,
    kw_mobi: String,
    kw_h5: String,
    kg_privilege: String,
    kg_song_info: String,
    kg_singer_pic: String,
    kg_lyric_search: String,
    kg_lyric_download: String,
    tx_musicu: String,
    tx_fcg_lyric: String,
    tx_pic_base: String,
    wy_lyric: String,
    wy_detail: String,
    mg_resource: String,
    mg_pic: String,
    mg_lyric: String,
}

impl Endpoints {
    fn production() -> Self {
        Self {
            kw_pic: "http://artistpicserver.kuwo.cn/pic.web".into(),
            kw_mobi: "http://mlyric.kuwo.cn/mobi.s".into(),
            kw_h5: "https://m.kuwo.cn/newh5/singles/songinfoandlrc".into(),
            kg_privilege: "http://media.store.kugou.com/v1/get_res_privilege".into(),
            kg_song_info: "http://m.kugou.com/app/i/getSongInfo.php".into(),
            kg_singer_pic: "http://tools.mobile.kugou.com/api/v1/singer_header/get_by_hash".into(),
            kg_lyric_search: "http://lyrics.kugou.com/search".into(),
            kg_lyric_download: "http://lyrics.kugou.com/download".into(),
            tx_musicu: "https://u.y.qq.com/cgi-bin/musicu.fcg".into(),
            tx_fcg_lyric: "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg".into(),
            tx_pic_base: "https://y.gtimg.cn/music/photo_new".into(),
            wy_lyric: "https://interface3.music.163.com/eapi/song/lyric/v1".into(),
            wy_detail: "https://interface3.music.163.com/eapi/v3/song/detail".into(),
            mg_resource: "https://c.musicapp.migu.cn/MIGUM2.0/v1.0/content/resourceinfo.do".into(),
            mg_pic: "https://music.migu.cn/v3/api/music/audioPlayer/getSongPic".into(),
            mg_lyric: "https://music.migu.cn/v3/api/music/audioPlayer/getLyric".into(),
        }
    }
}

fn as_http_url(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let url = Url::parse(trimmed).ok()?;
    (matches!(url.scheme(), "http" | "https") && url.host_str().is_some()).then_some(trimmed)
}

async fn read_bounded(mut response: Response, limit: usize) -> Option<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        return None;
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.ok()? {
        if chunk.len() > limit - bytes.len() {
            return None;
        }
        bytes.extend_from_slice(&chunk);
    }
    Some(bytes)
}

async fn fetch_bytes(req: RequestBuilder, limit: usize) -> Option<Vec<u8>> {
    let response = req.send().await.ok()?.error_for_status().ok()?;
    read_bounded(response, limit).await
}

async fn fetch_text(req: RequestBuilder) -> Option<String> {
    let bytes = fetch_bytes(req, API_LIMIT).await?;
    Some(match String::from_utf8(bytes) {
        Ok(s) => s,
        Err(err) => String::from_utf8_lossy(err.as_bytes()).into_owned(),
    })
}

async fn fetch_json(req: RequestBuilder) -> Option<Value> {
    let bytes = fetch_bytes(req, API_LIMIT).await?;
    serde_json::from_slice(&bytes).ok()
}

pub(super) async fn fetch_cover(
    client: &Client,
    url: &str,
    referer: Option<&str>,
) -> Option<(image::RgbaImage, Option<metadata::CoverPalette>)> {
    let url = as_http_url(url)?;
    let mut req = client.get(url).header("User-Agent", DESKTOP_UA);
    if let Some(referer) = referer {
        req = req.header("Referer", referer);
    }
    let bytes = fetch_bytes(req, COVER_LIMIT).await?;
    tokio::task::spawn_blocking(move || metadata::decode_cover(&bytes)).await.ok()?
}

pub fn inline_details(reference: &OnlineTrack) -> TrackDetails {
    let info = reference.info();
    let lyrics = info["lrc"]
        .as_str()
        .map(|raw| parse_lyrics(&fix_colon_time_labels(&decode_html(&normalize_lrc(raw)))))
        .unwrap_or_default();
    TrackDetails {
        lyrics,
        ..Default::default()
    }
}

pub async fn fetch_details(reference: &OnlineTrack, details: TrackDetails) -> Option<TrackDetails> {
    let client = super::api_client()?;
    fetch_details_with(client, &Endpoints::production(), reference, details).await
}

async fn fetch_details_with(
    client: &Client,
    endpoints: &Endpoints,
    reference: &OnlineTrack,
    mut details: TrackDetails,
) -> Option<TrackDetails> {
    let info = reference.info();
    let source = info["source"].as_str()?;
    let need_cover = details.cover.is_none();
    let need_lyrics = details.lyrics.is_empty();
    if !need_cover && !need_lyrics {
        return None;
    }

    let (cover, lyrics) = tokio::join!(
        async {
            if need_cover {
                resolve_and_fetch_cover(client, endpoints, source, &info).await
            } else {
                None
            }
        },
        async {
            if need_lyrics {
                resolve_lyrics(client, endpoints, source, &info).await
            } else {
                Vec::new()
            }
        },
    );

    let mut changed = false;
    if let Some((img, palette)) = cover {
        details.cover = Some(img);
        details.palette = palette;
        details.raw_cover = None;
        changed = true;
    }
    if !lyrics.is_empty() {
        details.lyrics = lyrics;
        changed = true;
    }
    changed.then_some(details)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let cleaned: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    if cleaned.is_empty() {
        return None;
    }
    BASE64_LENIENT.decode(&cleaned).ok()
}

fn decode_base64_utf8(input: &str) -> Option<String> {
    String::from_utf8(decode_base64(input)?).ok()
}

fn zlib_decompress(input: &[u8]) -> Option<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(input).take((ZLIB_LIMIT + 1) as u64);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).ok()?;
    (out.len() <= ZLIB_LIMIT).then_some(out)
}

fn normalize_lrc(raw: &str) -> String {
    raw.trim_start_matches('\u{feff}').replace("\r\n", "\n").replace('\r', "\n").trim().to_owned()
}

fn format_ms(ms: u64, pad3: bool) -> String {
    let frac = ms % 1000;
    let total_secs = ms / 1000;
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    if pad3 {
        format!("[{minutes:02}:{seconds:02}.{frac:03}]")
    } else {
        format!("[{minutes:02}:{seconds:02}.{:02}]", frac / 10)
    }
}

fn format_seconds(secs: f64) -> Option<String> {
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(format_ms((secs * 1000.0).round() as u64, false))
}

fn parse_time_ms(label: &str) -> Option<u64> {
    let (mins, rest) = label.trim().split_once(':')?;
    let minutes = mins.parse::<u64>().ok()?;
    let (secs_str, frac_str) =
        rest.split_once('.').or_else(|| rest.split_once(':')).unwrap_or((rest, ""));
    let seconds = secs_str.parse::<u64>().ok()?;
    let frac = if frac_str.is_empty() {
        0
    } else {
        let mut padded = frac_str.chars().take(3).collect::<String>();
        while padded.len() < 3 {
            padded.push('0');
        }
        padded.parse::<u64>().ok()?
    };
    minutes.checked_mul(60_000)?.checked_add(seconds.checked_mul(1000)?)?.checked_add(frac)
}

fn combine_lyrics(primary: &str, translation: &str) -> Vec<LyricLine> {
    let primary_lines = parse_lyrics(primary);
    if primary_lines.is_empty() {
        return Vec::new();
    }
    let translation = normalize_lrc(translation);
    if translation.is_empty() {
        return primary_lines;
    }
    parse_lyrics(&format!("{primary}\n{translation}"))
}

// --- Cover art resolution ---

fn cover_referer(source: &str) -> Option<&'static str> {
    match source {
        "tx" => Some("https://y.qq.com/"),
        "mg" => Some("https://music.migu.cn/"),
        _ => None,
    }
}

fn normalize_kg_img(raw: &str, size: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let with_scheme = if trimmed.starts_with("//") {
        format!("http:{trimmed}")
    } else {
        trimmed.to_owned()
    };
    let replaced = with_scheme.replace("{size}", size);
    as_http_url(&replaced).map(str::to_owned)
}

async fn resolve_and_fetch_cover(
    client: &Client,
    endpoints: &Endpoints,
    source: &str,
    info: &Value,
) -> Option<(image::RgbaImage, Option<metadata::CoverPalette>)> {
    let referer = cover_referer(source);
    let mut tried = Vec::new();

    if let Some(raw) = info["img"].as_str() {
        let candidate = match source {
            "kg" => normalize_kg_img(raw, "400"),
            "mg" => normalize_mg_img(raw),
            _ => as_http_url(raw).map(str::to_owned),
        };
        if let Some(url) = candidate {
            if let Some(img) = fetch_cover(client, &url, referer).await {
                return Some(img);
            }
            tried.push(url);
        }
    }

    let candidates = match source {
        "kw" => resolve_kw_cover_urls(client, endpoints, info).await,
        "kg" => resolve_kg_cover_urls(client, endpoints, info).await,
        "tx" => resolve_tx_cover_urls(client, endpoints, info, &tried).await,
        "wy" => resolve_wy_cover_urls(client, endpoints, info).await,
        "mg" => resolve_mg_cover_urls(client, endpoints, info).await,
        _ => Vec::new(),
    };

    for url in candidates {
        if tried.contains(&url) {
            continue;
        }
        if let Some(img) = fetch_cover(client, &url, referer).await {
            return Some(img);
        }
        tried.push(url);
    }
    None
}

async fn resolve_kw_cover_urls(
    client: &Client,
    endpoints: &Endpoints,
    info: &Value,
) -> Vec<String> {
    let Some(songmid) = id_string(&info["songmid"]) else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    if let Some(text) = fetch_text(client.get(&endpoints.kw_pic).query(&[
        ("corp", "kuwo"),
        ("type", "rid_pic"),
        ("pictype", "url"),
        ("content", "list"),
        ("size", "500"),
        ("rid", songmid.as_str()),
    ]))
    .await
    {
        let trimmed = text.trim();
        if trimmed != "NO_PIC"
            && let Some(url) = as_http_url(trimmed)
        {
            urls.push(url.to_owned());
        }
    }
    if urls.is_empty()
        && let Some(body) = fetch_json(
            client
                .get(&endpoints.kw_h5)
                .query(&[("musicId", songmid.as_str())])
                .header("User-Agent", DESKTOP_UA),
        )
        .await
    {
        let songinfo = &body["data"]["songinfo"];
        for key in ["pic", "albumpic"] {
            if let Some(url) = songinfo[key].as_str().and_then(as_http_url) {
                urls.push(url.to_owned());
            }
        }
    }
    urls
}

async fn resolve_kg_cover_urls(
    client: &Client,
    endpoints: &Endpoints,
    info: &Value,
) -> Vec<String> {
    let Some(hash) = info["hash"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    let mut urls = Vec::new();
    let album_id = id_string(&info["albumId"]).unwrap_or_else(|| "0".into());
    let album_audio_id = number(&info["albumAudioId"]).unwrap_or_default();
    let singer = info["singer"].as_str().unwrap_or_default();
    let name = info["name"].as_str().unwrap_or_default();
    let privilege_body = json!({
        "appid": 1001,
        "area_code": "1",
        "behavior": "play",
        "clientver": "9020",
        "need_hash_offset": 1,
        "relate": 1,
        "resource": [{
            "album_audio_id": album_audio_id,
            "album_id": album_id,
            "hash": hash,
            "id": 0,
            "name": format!("{singer} - {name}.mp3"),
            "type": "audio",
        }],
        "token": "",
        "userid": 262643156,
        "vip": 1,
    });
    if let Some(body) = fetch_json(
        client.post(&endpoints.kg_privilege).header("User-Agent", KG_UA).json(&privilege_body),
    )
    .await
        && let Some(first) = body["data"].as_array().and_then(|arr| arr.first())
    {
        let best_size = first["info"]["imgsize"]
            .as_array()
            .and_then(|sizes| sizes.iter().filter_map(number).max())
            .map(|n| n.to_string())
            .unwrap_or_else(|| "400".into());
        for raw in [first["info"]["image"].as_str(), first["trans_param"]["union_cover"].as_str()]
            .into_iter()
            .flatten()
        {
            if let Some(url) = normalize_kg_img(raw, &best_size) {
                urls.push(url);
            }
        }
    }
    if urls.is_empty()
        && let Some(body) = fetch_json(
            client
                .get(&endpoints.kg_song_info)
                .query(&[("cmd", "playInfo"), ("hash", hash)])
                .header("User-Agent", DESKTOP_UA),
        )
        .await
    {
        for raw in [
            body["album_img"].as_str(),
            body["imgUrl"].as_str(),
            body["trans_param"]["union_cover"].as_str(),
        ]
        .into_iter()
        .flatten()
        {
            if let Some(url) = normalize_kg_img(raw, "400") {
                urls.push(url);
            }
        }
    }
    if urls.is_empty()
        && let Some(body) = fetch_json(
            client
                .get(&endpoints.kg_singer_pic)
                .query(&[("hash", hash), ("size", "400"), ("format", "json")])
                .header("User-Agent", DESKTOP_UA),
        )
        .await
        && let Some(url) = body["url"].as_str().and_then(as_http_url)
    {
        urls.push(url.to_owned());
    }
    urls
}

async fn resolve_tx_cover_urls(
    client: &Client,
    endpoints: &Endpoints,
    info: &Value,
    tried: &[String],
) -> Vec<String> {
    let mut urls = Vec::new();
    let album_mid = info["albumMid"]
        .as_str()
        .or_else(|| info["albumId"].as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "空");
    if let Some(mid) = album_mid {
        let candidate = format!("{}/T002R500x500M000{mid}.jpg", endpoints.tx_pic_base);
        if !tried.contains(&candidate) {
            urls.push(candidate);
        }
    }
    if let Some(songmid) = id_string(&info["songmid"])
        && urls.is_empty()
        && let Some(body) = fetch_json(
            client
                .post(&endpoints.tx_musicu)
                .header("Referer", "https://y.qq.com")
                .header("User-Agent", TX_UA)
                .json(&json!({
                    "comm": {"ct": "19", "cv": "1859", "uin": "0"},
                    "req": {
                        "module": "music.pf_song_detail_svr",
                        "method": "get_song_detail_yqq",
                        "param": {"song_mid": songmid, "song_type": 0},
                    },
                })),
        )
        .await
        && number(&body["code"]) == Some(0)
        && number(&body["req"]["code"]) == Some(0)
    {
        let track_info = &body["req"]["data"]["track_info"];
        if let Some(mid) = track_info["album"]["mid"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty() && *s != "空")
        {
            urls.push(format!("{}/T002R500x500M000{mid}.jpg", endpoints.tx_pic_base));
        }
        if let Some(mid) = track_info["singer"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|s| s["mid"].as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            urls.push(format!("{}/T001R500x500M000{mid}.jpg", endpoints.tx_pic_base));
        }
    }
    urls
}

async fn resolve_wy_cover_urls(
    client: &Client,
    endpoints: &Endpoints,
    info: &Value,
) -> Vec<String> {
    let Some(song_id) = number(&info["songmid"]) else {
        return Vec::new();
    };
    let Ok(params) =
        wy_eapi("/api/v3/song/detail", &json!({"c": format!("[{{\"id\":{song_id}}}]")}))
    else {
        return Vec::new();
    };
    let Some(body) = fetch_json(
        client
            .post(&endpoints.wy_detail)
            .header("origin", "https://music.163.com")
            .header("User-Agent", DESKTOP_UA)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("params={params}")),
    )
    .await
    else {
        return Vec::new();
    };
    if number(&body["code"]) != Some(200) {
        return Vec::new();
    }
    body["songs"]
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|song| song["al"]["picUrl"].as_str())
        .and_then(as_http_url)
        .map(|u| vec![u.to_owned()])
        .unwrap_or_default()
}

fn extract_mg_resource_covers(resource: &Value) -> Vec<String> {
    let mut urls = Vec::new();
    for list_key in ["albumImgs", "imgItems"] {
        if let Some(items) = resource[list_key].as_array() {
            for preferred in ["03", "02", "01"] {
                if let Some(url) = items
                    .iter()
                    .find(|item| item["imgSizeType"].as_str() == Some(preferred))
                    .and_then(|item| item["img"].as_str().or_else(|| item["imgUrl"].as_str()))
                    .and_then(normalize_mg_img)
                {
                    urls.push(url);
                }
            }
            for item in items.iter().rev() {
                if let Some(url) = item["img"]
                    .as_str()
                    .or_else(|| item["imgUrl"].as_str())
                    .and_then(normalize_mg_img)
                    && !urls.contains(&url)
                {
                    urls.push(url);
                }
            }
        }
    }
    urls
}

async fn resolve_mg_cover_urls(
    client: &Client,
    endpoints: &Endpoints,
    info: &Value,
) -> Vec<String> {
    let mut urls = Vec::new();
    if let Some(resource_id) =
        id_string(&info["copyrightId"]).or_else(|| id_string(&info["songmid"]))
        && let Some(body) = fetch_json(
            client
                .post(&endpoints.mg_resource)
                .query(&[("resourceType", "2")])
                .header("Referer", "https://app.c.nf.migu.cn/")
                .header("User-Agent", MG_UA)
                .header("channel", "0146921")
                .form(&[("resourceId", resource_id.as_str())]),
        )
        .await
        && let Some(first) = body["resource"].as_array().and_then(|arr| arr.first())
    {
        urls.extend(extract_mg_resource_covers(first));
    }
    if urls.is_empty()
        && let Some(song_id) = id_string(&info["songmid"])
        && let Some(body) = fetch_json(
            client
                .get(&endpoints.mg_pic)
                .query(&[("songId", song_id.as_str())])
                .header("Referer", "https://music.migu.cn/")
                .header("User-Agent", DESKTOP_UA),
        )
        .await
    {
        for key in ["largePic", "mediumPic", "smallPic"] {
            if let Some(url) = body[key].as_str().and_then(normalize_mg_img)
                && !urls.contains(&url)
            {
                urls.push(url);
            }
        }
    }
    urls
}

// --- Lyrics resolution ---

async fn resolve_lyrics(
    client: &Client,
    endpoints: &Endpoints,
    source: &str,
    info: &Value,
) -> Vec<LyricLine> {
    match source {
        "kw" => get_kw_lyrics(client, endpoints, info).await,
        "kg" => get_kg_lyrics(client, endpoints, info).await,
        "tx" => get_tx_lyrics(client, endpoints, info).await,
        "wy" => get_wy_lyrics(client, endpoints, info).await,
        "mg" => get_mg_lyrics(client, endpoints, info).await,
        _ => Vec::new(),
    }
}

// --- Kuwo (kw) ---

#[derive(Clone)]
struct TimedLine {
    time: String,
    text: String,
}

fn strip_angle_word_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('>')
            && is_kw_word_tag(&after[..end])
        {
            rest = &after[end + 1..];
            continue;
        }
        out.push('<');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn is_kw_word_tag(tag: &str) -> bool {
    let mut parts = tag.split(',');
    let (Some(a), Some(b)) = (parts.next(), parts.next()) else {
        return false;
    };
    let c_ok = match parts.next() {
        Some(c) => parts.next().is_none() && c.parse::<i64>().is_ok(),
        None => true,
    };
    a.parse::<i64>().is_ok() && b.parse::<i64>().is_ok() && c_ok
}

fn starts_with_kw_word_tag(text: &str) -> bool {
    let Some(rest) = text.strip_prefix('<') else {
        return false;
    };
    rest.split_once('>').is_some_and(|(tag, _)| is_kw_word_tag(tag))
}

fn decode_kw_mobi_lyric(raw: &[u8]) -> Option<String> {
    if raw.len() < 10 || !raw[..10].eq_ignore_ascii_case(b"tp=content") {
        return None;
    }
    let sep = raw.windows(4).position(|w| w == b"\r\n\r\n")?;
    let header = String::from_utf8_lossy(&raw[..sep]).to_ascii_lowercase();
    let inflated = zlib_decompress(&raw[sep + 4..])?;
    let plain_lrcx_0 = header.contains("lrcx=0");
    if !plain_lrcx_0
        && let Ok(b64_text) = std::str::from_utf8(&inflated)
        && let Some(mut decoded) = decode_base64(b64_text)
    {
        for (i, byte) in decoded.iter_mut().enumerate() {
            *byte ^= KW_XOR_KEY[i % KW_XOR_KEY.len()];
        }
        if let Ok(text) = String::from_utf8(decoded) {
            return Some(text);
        }
    }
    String::from_utf8(inflated).ok()
}

fn sort_kw_lines(
    arr: Vec<TimedLine>,
    allow_trailing_trans: bool,
) -> (Vec<TimedLine>, Vec<TimedLine>) {
    let mut seen = std::collections::HashSet::new();
    let mut lrc: Vec<TimedLine> = Vec::new();
    let mut lrc_t: Vec<TimedLine> = Vec::new();
    let mut is_lyricx = false;

    for item in &arr {
        if !is_lyricx && starts_with_kw_word_tag(&item.text) {
            is_lyricx = true;
        }
        if seen.contains(&item.time) {
            if lrc.len() < 2 {
                continue;
            }
            let mut trans = lrc.pop().unwrap();
            trans.time = lrc.last().unwrap().time.clone();
            lrc_t.push(trans);
            lrc.push(item.clone());
        } else {
            seen.insert(item.time.clone());
            lrc.push(item.clone());
        }
    }

    if allow_trailing_trans && !lrc_t.is_empty() && lrc.len() > lrc_t.len() {
        let mut trans = lrc.pop().unwrap();
        trans.time = lrc.last().unwrap().time.clone();
        lrc_t.push(trans);
    }

    if !allow_trailing_trans
        && !is_lyricx
        && (lrc_t.len() as f64) > (lrc.len() as f64) * 0.3
        && lrc.len().saturating_sub(lrc_t.len()) > 6
    {
        return (arr, Vec::new());
    }

    (lrc, lrc_t)
}

fn render_kw_lines(lines: &[TimedLine], strip_word_tags: bool) -> String {
    lines
        .iter()
        .filter_map(|item| {
            let cleaned = if strip_word_tags {
                strip_angle_word_tags(&item.text)
            } else {
                item.text.clone()
            };
            let cleaned = decode_html(cleaned.trim());
            let cleaned = cleaned.trim();
            (!cleaned.is_empty()).then(|| format!("[{}]{cleaned}", item.time))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn split_bracket_timestamp(line: &str) -> Option<(&str, &str)> {
    let (stamp, rest) = line.strip_prefix('[')?.split_once(']')?;
    if !stamp.is_empty()
        && stamp.bytes().all(|b| b.is_ascii_digit() || b == b':' || b == b'.')
        && stamp.contains(':')
    {
        Some((stamp, rest))
    } else {
        None
    }
}

fn parse_kw_mobi_text(text: &str) -> Vec<LyricLine> {
    let mut items = Vec::new();
    for raw_line in normalize_lrc(text).lines() {
        let line = raw_line.trim();
        if let Some((stamp, rest)) = split_bracket_timestamp(line) {
            let mut time = stamp.to_owned();
            if time.split_once('.').is_some_and(|(_, frac)| frac.len() == 2) {
                time.push('0');
            }
            items.push(TimedLine {
                time,
                text: rest.trim().to_owned(),
            });
        }
    }
    if items.is_empty() {
        return Vec::new();
    }
    let (lrc, lrc_t) = sort_kw_lines(items, false);
    let primary = render_kw_lines(&lrc, true);
    let translation = render_kw_lines(&lrc_t, true);
    combine_lyrics(&primary, &translation)
}

fn parse_kw_h5_result(body: &Value) -> Vec<LyricLine> {
    let Some(list) = body["data"]["lrclist"].as_array() else {
        return Vec::new();
    };
    let mut items = Vec::new();
    for row in list {
        let sec = row["time"].as_f64().or_else(|| row["time"].as_str()?.trim().parse::<f64>().ok());
        let Some(label) = sec.and_then(format_seconds) else {
            continue;
        };
        items.push(TimedLine {
            time: label[1..label.len() - 1].to_owned(),
            text: row["lineLyric"].as_str().unwrap_or_default().to_owned(),
        });
    }
    if items.is_empty() {
        return Vec::new();
    }
    let (lrc, lrc_t) = sort_kw_lines(items, true);
    let primary = render_kw_lines(&lrc, false);
    let translation = render_kw_lines(&lrc_t, false);
    combine_lyrics(&primary, &translation)
}

async fn get_kw_lyrics(client: &Client, endpoints: &Endpoints, info: &Value) -> Vec<LyricLine> {
    let Some(songmid) = id_string(&info["songmid"]) else {
        return Vec::new();
    };
    if let Some(raw) = fetch_bytes(
        client
            .get(&endpoints.kw_mobi)
            .query(&[
                ("f", "web"),
                ("type", "lyric"),
                ("lrcx", "1"),
                ("rid", songmid.as_str()),
                ("encode", "utf8"),
            ])
            .header("User-Agent", DESKTOP_UA),
        API_LIMIT,
    )
    .await
        && let Some(decoded) = decode_kw_mobi_lyric(&raw)
    {
        let lines = parse_kw_mobi_text(&decoded);
        if !lines.is_empty() {
            return lines;
        }
    }
    if let Some(body) = fetch_json(
        client
            .get(&endpoints.kw_h5)
            .query(&[("musicId", songmid.as_str())])
            .header("User-Agent", DESKTOP_UA),
    )
    .await
    {
        return parse_kw_h5_result(&body);
    }
    Vec::new()
}

// --- Kugou (kg) ---

fn duration_ms_from_info(info: &Value) -> u64 {
    if let Some(sec) = number(&info["_interval"]).filter(|&n| n > 0)
        && let Some(ms) = sec.checked_mul(1000)
    {
        return ms;
    }
    if let Some((m, s)) = info["interval"].as_str().and_then(|v| v.split_once(':'))
        && let (Ok(m), Ok(s)) = (m.trim().parse::<u64>(), s.trim().parse::<u64>())
        && let Some(ms) = m
            .checked_mul(60)
            .and_then(|sec| sec.checked_add(s))
            .and_then(|sec| sec.checked_mul(1000))
    {
        return ms;
    }
    0
}

fn strip_krc_word_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find('>') {
            let tag = &after[..end];
            let mut parts = tag.split(',');
            let valid = parts.next().is_some_and(|p| p.parse::<u64>().is_ok())
                && parts.next().is_some_and(|p| p.parse::<u64>().is_ok())
                && parts.next().is_some_and(|p| p.parse::<u64>().is_ok())
                && parts.next().is_none();
            if valid {
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('<');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn decode_kg_krc_content(content_b64: &str) -> Vec<LyricLine> {
    let Some(raw) = decode_base64(content_b64) else {
        return Vec::new();
    };
    if raw.len() <= 4 {
        return Vec::new();
    }
    let payload: Vec<u8> =
        raw[4..].iter().enumerate().map(|(i, b)| b ^ KG_KRC_KEY[i % KG_KRC_KEY.len()]).collect();
    let Some(inflated) = zlib_decompress(&payload) else {
        return Vec::new();
    };
    let Ok(text) = String::from_utf8(inflated) else {
        return Vec::new();
    };
    parse_kg_krc_text(&text)
}

fn parse_kg_krc_text(raw: &str) -> Vec<LyricLine> {
    let text = normalize_lrc(raw);
    let mut trans_rows: Option<Vec<Vec<String>>> = None;

    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("[language:")
            && let Some(b64) = rest.strip_suffix(']')
            && let Some(json_str) = decode_base64_utf8(b64)
            && let Ok(parsed) = serde_json::from_str::<Value>(&json_str)
            && let Some(content) = parsed["content"].as_array()
            && let Some(trans_item) = content.iter().find(|item| number(&item["type"]) == Some(1))
            && let Some(rows) = trans_item["lyricContent"].as_array()
        {
            trans_rows = Some(
                rows.iter()
                    .map(|row| {
                        row.as_array()
                            .map(|parts| {
                                parts.iter().filter_map(|p| p.as_str().map(str::to_owned)).collect()
                            })
                            .unwrap_or_default()
                    })
                    .collect(),
            );
            break;
        }
    }

    let mut primary_lines = Vec::new();
    let mut trans_lines = Vec::new();
    let mut line_index = 0usize;

    for line in text.lines() {
        let Some((head, body)) = line.trim().strip_prefix('[').and_then(|s| s.split_once(']'))
        else {
            continue;
        };
        let Some((start_str, dur_str)) = head.split_once(',') else {
            continue;
        };
        let (Ok(start_ms), Ok(_)) = (start_str.parse::<u64>(), dur_str.parse::<u64>()) else {
            continue;
        };
        let time_tag = format_ms(start_ms, true);
        let cleaned = decode_html(strip_krc_word_tags(body).trim());
        primary_lines.push(format!("{time_tag}{}", cleaned.trim()));
        if let Some(rows) = &trans_rows
            && let Some(parts) = rows.get(line_index)
        {
            let joined = decode_html(&parts.concat());
            let trimmed = joined.trim();
            if !trimmed.is_empty() {
                trans_lines.push(format!("{time_tag}{trimmed}"));
            }
        }
        line_index += 1;
    }

    combine_lyrics(&primary_lines.join("\n"), &trans_lines.join("\n"))
}

async fn download_kg_lyric(
    client: &Client,
    endpoints: &Endpoints,
    id: &str,
    access_key: &str,
    fmt: &str,
) -> Vec<LyricLine> {
    let Some(body) = fetch_json(
        client
            .get(&endpoints.kg_lyric_download)
            .query(&[
                ("ver", "1"),
                ("client", "pc"),
                ("id", id),
                ("accesskey", access_key),
                ("fmt", fmt),
                ("charset", "utf8"),
            ])
            .header("KG-RC", "1")
            .header("KG-THash", "expand_search_manager.cpp:852736169:451")
            .header("User-Agent", KG_UA),
    )
    .await
    else {
        return Vec::new();
    };
    let Some(content) = body["content"].as_str().filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    match body["fmt"].as_str() {
        Some("krc") => decode_kg_krc_content(content),
        Some("lrc") => decode_base64_utf8(content)
            .map(|decoded| parse_lyrics(&decode_html(&normalize_lrc(&decoded))))
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

async fn get_kg_lyrics(client: &Client, endpoints: &Endpoints, info: &Value) -> Vec<LyricLine> {
    let name = info["name"].as_str().unwrap_or_default().trim();
    let hash = info["hash"].as_str().unwrap_or_default().trim();
    if name.is_empty() && hash.is_empty() {
        return Vec::new();
    }
    let timelength = duration_ms_from_info(info).to_string();
    let Some(search) = fetch_json(
        client
            .get(&endpoints.kg_lyric_search)
            .query(&[
                ("ver", "1"),
                ("man", "yes"),
                ("client", "pc"),
                ("keyword", name),
                ("hash", hash),
                ("timelength", timelength.as_str()),
                ("lrctxt", "1"),
            ])
            .header("KG-RC", "1")
            .header("KG-THash", "expand_search_manager.cpp:852736169:451")
            .header("User-Agent", KG_UA),
    )
    .await
    else {
        return Vec::new();
    };
    let Some(candidate) = search["candidates"].as_array().and_then(|arr| arr.first()) else {
        return Vec::new();
    };
    let (Some(id), Some(access_key)) = (
        id_string(&candidate["id"]),
        candidate["accesskey"].as_str().map(str::trim).filter(|s| !s.is_empty()),
    ) else {
        return Vec::new();
    };
    let prefer_krc =
        number(&candidate["krctype"]) == Some(1) && number(&candidate["contenttype"]) != Some(1);
    if prefer_krc {
        let lines = download_kg_lyric(client, endpoints, &id, access_key, "krc").await;
        if !lines.is_empty() {
            return lines;
        }
    }
    download_kg_lyric(client, endpoints, &id, access_key, "lrc").await
}

// --- QQ Music (tx) ---

fn is_placeholder_tx_trans(line: &str) -> bool {
    let mut rest = line.trim();
    let mut had_stamp = false;
    while let Some((_, tail)) = split_bracket_timestamp(rest) {
        had_stamp = true;
        rest = tail;
    }
    had_stamp && rest.trim() == "//"
}

fn clean_tx_translation(raw: &str) -> String {
    normalize_lrc(raw)
        .lines()
        .filter(|line| !is_placeholder_tx_trans(line))
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_tx_pair(lyric_b64: Option<&str>, trans_b64: Option<&str>) -> Vec<LyricLine> {
    let Some(lyric) = lyric_b64.and_then(decode_base64_utf8) else {
        return Vec::new();
    };
    let primary = decode_html(&normalize_lrc(&lyric));
    let trans = trans_b64
        .and_then(decode_base64_utf8)
        .map(|s| clean_tx_translation(&decode_html(&s)))
        .unwrap_or_default();
    combine_lyrics(&primary, &trans)
}

async fn get_tx_lyrics(client: &Client, endpoints: &Endpoints, info: &Value) -> Vec<LyricLine> {
    if let Some(song_id) = number(&info["songId"]).filter(|&id| id > 0)
        && let Some(body) = fetch_json(
            client
                .post(&endpoints.tx_musicu)
                .header("Referer", "https://y.qq.com")
                .header("User-Agent", TX_UA)
                .json(&json!({
                    "comm": {"ct": "19", "cv": "1859", "uin": "0"},
                    "req": {
                        "method": "GetPlayLyricInfo",
                        "module": "music.musichallSong.PlayLyricInfo",
                        "param": {
                            "format": "json",
                            "crypt": 0,
                            "ct": 19,
                            "cv": 1873,
                            "interval": 0,
                            "lrc_t": 0,
                            "qrc": 0,
                            "qrc_t": 0,
                            "roma": 0,
                            "roma_t": 0,
                            "songID": song_id,
                            "trans": 1,
                            "trans_t": 0,
                            "type": -1,
                        },
                    },
                })),
        )
        .await
        && number(&body["code"]) == Some(0)
        && number(&body["req"]["code"]) == Some(0)
    {
        let data = &body["req"]["data"];
        let lines = parse_tx_pair(data["lyric"].as_str(), data["trans"].as_str());
        if !lines.is_empty() {
            return lines;
        }
    }

    let Some(songmid) = id_string(&info["songmid"]) else {
        return Vec::new();
    };
    if let Some(body) = fetch_json(
        client
            .get(&endpoints.tx_fcg_lyric)
            .query(&[
                ("songmid", songmid.as_str()),
                ("g_tk", "5381"),
                ("loginUin", "0"),
                ("hostUin", "0"),
                ("format", "json"),
                ("inCharset", "utf8"),
                ("outCharset", "utf-8"),
                ("platform", "yqq"),
            ])
            .header("Referer", "https://y.qq.com/portal/player.html")
            .header("User-Agent", TX_UA),
    )
    .await
        && number(&body["code"]) == Some(0)
    {
        return parse_tx_pair(body["lyric"].as_str(), body["trans"].as_str());
    }
    Vec::new()
}

// --- NetEase Cloud Music (wy) ---

fn parse_colon_stamp(bytes: &[u8]) -> Option<(usize, usize)> {
    // Match `[\d+:\d+:\d+]`, returning `(second_colon_offset, total_len)`.
    if *bytes.first()? != b'[' {
        return None;
    }
    let take_digits = |start: usize| -> Option<usize> {
        let count = bytes.get(start..)?.iter().take_while(|b| b.is_ascii_digit()).count();
        (count > 0).then_some(start + count)
    };
    let mut p = take_digits(1)?;
    if bytes.get(p) != Some(&b':') {
        return None;
    }
    p = take_digits(p + 1)?;
    if bytes.get(p) != Some(&b':') {
        return None;
    }
    let second_colon = p;
    p = take_digits(p + 1)?;
    (bytes.get(p) == Some(&b']')).then_some((second_colon, p + 1))
}

fn fix_colon_time_labels(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if let Some((colon, len)) = parse_colon_stamp(&bytes[i..]) {
            out.push_str(&text[i..i + colon]);
            out.push('.');
            out.push_str(&text[i + colon + 1..i + len]);
            i += len;
            continue;
        }
        let ch = text[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn has_frac_digits(text: &str, digits: usize) -> bool {
    let mut rest = text;
    while let Some(pos) = rest.find('[') {
        rest = &rest[pos + 1..];
        if let Some((stamp, tail)) = rest.split_once(']') {
            if let Some((_, sec_part)) = stamp.split_once(':')
                && let Some((_, frac)) = sec_part.split_once('.')
                && frac.len() == digits
                && frac.bytes().all(|b| b.is_ascii_digit())
            {
                return true;
            }
            rest = tail;
        } else {
            break;
        }
    }
    false
}

fn parse_wy_header_info(raw: &str) -> Vec<String> {
    let normalized = normalize_lrc(raw);
    if normalized.is_empty() {
        return Vec::new();
    }
    let pad3 = has_frac_digits(&normalized, 3) || !has_frac_digits(&normalized, 2);
    normalized
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if !trimmed.starts_with("{\"") {
                return (!trimmed.is_empty()).then(|| trimmed.to_owned());
            }
            let info: Value = serde_json::from_str(trimmed).ok()?;
            let ms = info["t"].as_u64()?;
            let parts = info["c"].as_array()?;
            let text: String = parts.iter().filter_map(|p| p["tx"].as_str()).collect();
            let text = text.trim();
            (!text.is_empty()).then(|| format!("{}{text}", format_ms(ms, pad3)))
        })
        .collect()
}

fn strip_yrc_word_tags(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('(') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        if let Some(end) = after.find(')') {
            let tag = &after[..end];
            let mut parts = tag.split(',');
            let valid = parts.next().is_some_and(|p| p.parse::<u64>().is_ok())
                && parts.next().is_some_and(|p| p.parse::<u64>().is_ok())
                && parts.next().is_some_and(|p| p.parse::<u64>().is_ok())
                && parts.next().is_none();
            if valid {
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('(');
        rest = after;
    }
    out.push_str(rest);
    out
}

fn parse_yrc_lines(lines: &[String]) -> String {
    lines
        .iter()
        .filter_map(|raw_line| {
            let line = raw_line.trim();
            let (head, body) = line.strip_prefix('[').and_then(|s| s.split_once(']'))?;
            let (start_str, dur_str) = head.split_once(',')?;
            let (Ok(start_ms), Ok(_)) = (start_str.parse::<u64>(), dur_str.parse::<u64>()) else {
                return None;
            };
            let text = strip_yrc_word_tags(body);
            Some(format!("{}{}", format_ms(start_ms, true), text.trim()))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn align_translation_time_tags(primary_lrc: &str, trans_lrc: &str) -> String {
    let mut primary_lines: std::collections::VecDeque<&str> = primary_lrc.lines().collect();
    let mut aligned = Vec::new();
    let mut skipped = Vec::new();

    for line in trans_lrc.lines() {
        let Some((trans_stamp, words)) = split_bracket_timestamp(line.trim()) else {
            continue;
        };
        let words = words.trim();
        if words.is_empty() {
            continue;
        }
        let Some(t1) = parse_time_ms(trans_stamp) else {
            continue;
        };
        while let Some(candidate) = primary_lines.pop_front() {
            let Some((primary_stamp, _)) = split_bracket_timestamp(candidate.trim()) else {
                continue;
            };
            let Some(t2) = parse_time_ms(primary_stamp) else {
                continue;
            };
            if t1.abs_diff(t2) < 100 {
                aligned.push(format!("[{primary_stamp}]{words}"));
                break;
            }
            skipped.push(candidate);
        }
        while let Some(item) = skipped.pop() {
            primary_lines.push_front(item);
        }
    }
    aligned.join("\n")
}

fn parse_wy_lyric_response(body: &Value) -> Vec<LyricLine> {
    let raw_lrc = body["lrc"]["lyric"].as_str().map(fix_colon_time_labels).unwrap_or_default();
    let raw_tlrc = body["tlyric"]["lyric"].as_str().map(fix_colon_time_labels).unwrap_or_default();
    let raw_yrc = body["yrc"]["lyric"].as_str().unwrap_or_default();
    let raw_ytlrc = body["ytlrc"]["lyric"].as_str().unwrap_or_default();

    if !raw_yrc.is_empty() {
        let lines = parse_wy_header_info(raw_yrc);
        if !lines.is_empty() {
            let yrc_body = parse_yrc_lines(&lines);
            let headers = lines
                .iter()
                .filter(|line| split_bracket_timestamp(line.trim()).is_some())
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            let combined = [headers.as_str(), yrc_body.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            if !yrc_body.is_empty() && !parse_lyrics(&combined).is_empty() {
                let trans_source = if !raw_ytlrc.is_empty() {
                    raw_ytlrc
                } else {
                    raw_tlrc.as_str()
                };
                let trans = if !trans_source.is_empty() {
                    let trans_lines = parse_wy_header_info(trans_source).join("\n");
                    let aligned = align_translation_time_tags(&yrc_body, &trans_lines);
                    if aligned.is_empty() {
                        trans_lines
                    } else {
                        aligned
                    }
                } else {
                    String::new()
                };
                return combine_lyrics(&combined, &trans);
            }
        }
    }

    if raw_lrc.is_empty() {
        return Vec::new();
    }
    let primary = parse_wy_header_info(&raw_lrc).join("\n");
    let trans = if raw_tlrc.is_empty() {
        String::new()
    } else {
        parse_wy_header_info(&raw_tlrc).join("\n")
    };
    combine_lyrics(&primary, &trans)
}

async fn get_wy_lyrics(client: &Client, endpoints: &Endpoints, info: &Value) -> Vec<LyricLine> {
    if id_string(&info["songmid"]).is_none() {
        return Vec::new();
    }
    let Ok(params) = wy_eapi(
        "/api/song/lyric/v1",
        &json!({
            "id": info["songmid"],
            "cp": false,
            "tv": 0,
            "lv": 0,
            "rv": 0,
            "kv": 0,
            "yv": 0,
            "ytv": 0,
            "yrv": 0,
        }),
    ) else {
        return Vec::new();
    };
    let Some(body) = fetch_json(
        client
            .post(&endpoints.wy_lyric)
            .header("origin", "https://music.163.com")
            .header("User-Agent", DESKTOP_UA)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("params={params}")),
    )
    .await
    else {
        return Vec::new();
    };
    if number(&body["code"]) != Some(200) {
        return Vec::new();
    }
    parse_wy_lyric_response(&body)
}

// --- Migu (mg) ---

async fn get_mg_lyrics(client: &Client, endpoints: &Endpoints, info: &Value) -> Vec<LyricLine> {
    let mut lrc_url = info["lrcUrl"].as_str().and_then(as_http_url).map(str::to_owned);
    let mut trc_url = info["trcUrl"].as_str().and_then(as_http_url).map(str::to_owned);

    if lrc_url.is_none()
        && let Some(resource_id) =
            id_string(&info["copyrightId"]).or_else(|| id_string(&info["songmid"]))
        && let Some(body) = fetch_json(
            client
                .post(&endpoints.mg_resource)
                .query(&[("resourceType", "2")])
                .header("Referer", "https://app.c.nf.migu.cn/")
                .header("User-Agent", MG_UA)
                .header("channel", "0146921")
                .form(&[("resourceId", resource_id.as_str())]),
        )
        .await
        && let Some(first) = body["resource"].as_array().and_then(|arr| arr.first())
    {
        lrc_url = first["lrcUrl"].as_str().and_then(as_http_url).map(str::to_owned);
        if trc_url.is_none() {
            trc_url = first["trcUrl"].as_str().and_then(as_http_url).map(str::to_owned);
        }
    }

    if let Some(lrc_url) = lrc_url {
        let lrc_req = client
            .get(&lrc_url)
            .header("Referer", "https://app.c.nf.migu.cn/")
            .header("User-Agent", MG_UA)
            .header("channel", "0146921");
        let trc_fut = async {
            if let Some(url) = trc_url.as_deref() {
                fetch_text(
                    client
                        .get(url)
                        .header("Referer", "https://app.c.nf.migu.cn/")
                        .header("User-Agent", MG_UA)
                        .header("channel", "0146921"),
                )
                .await
            } else {
                None
            }
        };
        let (raw_lrc, raw_trc) = tokio::join!(fetch_text(lrc_req), trc_fut);
        if let Some(raw_lrc) = raw_lrc {
            let primary = decode_html(&normalize_lrc(&raw_lrc));
            let translation =
                raw_trc.as_deref().map(|s| decode_html(&normalize_lrc(s))).unwrap_or_default();
            let lines = combine_lyrics(&primary, &translation);
            if !lines.is_empty() {
                return lines;
            }
        }
    }

    if let Some(copyright_id) = id_string(&info["copyrightId"])
        && let Some(body) = fetch_json(
            client
                .get(&endpoints.mg_lyric)
                .query(&[("copyrightId", copyright_id.as_str())])
                .header("Referer", "https://music.migu.cn/")
                .header("User-Agent", DESKTOP_UA),
        )
        .await
        && body["returnCode"].as_str() == Some("000000")
        && let Some(lyric) = body["lyric"].as_str()
    {
        return parse_lyrics(&decode_html(&normalize_lrc(lyric)));
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{BufRead, BufReader, Cursor, Read, Write},
        net::TcpListener,
        sync::Arc,
        thread,
    };

    use base64::engine::general_purpose::STANDARD as BASE64;
    use flate2::{Compression, write::ZlibEncoder};

    use super::*;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap()
    }

    fn client() -> Client {
        Client::builder().no_proxy().build().unwrap()
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbaImage::new(w, h);
        img.put_pixel(0, 0, image::Rgba([10, 20, 30, 255]));
        let mut encoded = Vec::new();
        img.write_to(&mut Cursor::new(&mut encoded), image::ImageFormat::Png).unwrap();
        encoded
    }

    fn http_resp(status: &str, content_type: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn start_server(
        count: usize,
        handler: impl Fn(&str, &str) -> Vec<u8> + Send + Sync + 'static,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handler = Arc::new(handler);
        let join = thread::spawn(move || {
            for _ in 0..count {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut content_len = 0usize;
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    if line == "\r\n" || line == "\n" {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                    line.clear();
                }
                let mut body_bytes = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut body_bytes);
                }
                let body = String::from_utf8_lossy(&body_bytes);
                let resp = handler(&path, &body);
                let _ = stream.write_all(&resp);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}"), join)
    }

    fn endpoints_for(origin: &str) -> Endpoints {
        Endpoints {
            kw_pic: format!("{origin}/kw/pic"),
            kw_mobi: format!("{origin}/kw/mobi"),
            kw_h5: format!("{origin}/kw/h5"),
            kg_privilege: format!("{origin}/kg/privilege"),
            kg_song_info: format!("{origin}/kg/songinfo"),
            kg_singer_pic: format!("{origin}/kg/singerpic"),
            kg_lyric_search: format!("{origin}/kg/lrc_search"),
            kg_lyric_download: format!("{origin}/kg/lrc_download"),
            tx_musicu: format!("{origin}/tx/musicu"),
            tx_fcg_lyric: format!("{origin}/tx/fcg"),
            tx_pic_base: format!("{origin}/tx/pic"),
            wy_lyric: format!("{origin}/wy/lyric"),
            wy_detail: format!("{origin}/wy/detail"),
            mg_resource: format!("{origin}/mg/resource"),
            mg_pic: format!("{origin}/mg/pic"),
            mg_lyric: format!("{origin}/mg/lyric"),
        }
    }

    fn encode_kw_mobi(plain: &str) -> Vec<u8> {
        let mut xored = plain.as_bytes().to_vec();
        for (i, b) in xored.iter_mut().enumerate() {
            *b ^= KW_XOR_KEY[i % KW_XOR_KEY.len()];
        }
        let b64 = BASE64.encode(&xored);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b64.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut out = b"TP=content\r\nlrcx=1\r\n\r\n".to_vec();
        out.extend_from_slice(&compressed);
        out
    }

    fn encode_kg_krc(plain: &str) -> String {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(plain.as_bytes()).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut out = Vec::with_capacity(4 + compressed.len());
        out.extend_from_slice(b"krc1");
        for (i, b) in compressed.into_iter().enumerate() {
            out.push(b ^ KG_KRC_KEY[i % KG_KRC_KEY.len()]);
        }
        BASE64.encode(out)
    }

    #[test]
    fn kw_fetches_cover_and_decodes_mobi_and_h5_fallback() {
        let png = png_bytes(4, 4);
        let png2 = png.clone();
        let mobi2 = encode_kw_mobi(
            "[00:01.54]<100,-100>夢<200,50>ならば\n\
             [00:02.88]<0,0>如果只是一场梦\n\
             [00:02.88]<100,-100>どれほど&amp;よかった\n\
             [00:06.88]<0,0>那该有多好\n\
             [00:06.88]",
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let cover_url = format!("{origin}/cover.png");
        let server = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut hdr = String::new();
                while reader
                    .read_line(&mut hdr)
                    .is_ok_and(|n| n > 0 && hdr != "\r\n" && hdr != "\n")
                {
                    hdr.clear();
                }
                let resp = if path.starts_with("/kw/pic") {
                    http_resp("200 OK", "text/plain", cover_url.as_bytes())
                } else if path == "/cover.png" {
                    http_resp("200 OK", "image/png", &png2)
                } else {
                    http_resp("200 OK", "application/octet-stream", &mobi2)
                };
                stream.write_all(&resp).unwrap();
            }
        });
        let track = OnlineTrack::from_info(json!({
            "source": "kw", "name": "Lemon", "singer": "Artist", "songmid": "228908",
            "types": [{"type": "128k"}],
        }))
        .unwrap();
        let details = runtime()
            .block_on(fetch_details_with(
                &client(),
                &endpoints_for(&origin),
                &track,
                Default::default(),
            ))
            .unwrap();
        server.join().unwrap();
        assert_eq!(details.cover.unwrap().dimensions(), (4, 4));
        let texts: Vec<_> = details.lyrics.iter().map(|l| (l.time, l.text.as_str())).collect();
        assert_eq!(
            texts,
            vec![
                (1.54, "夢ならば"),
                (1.54, "如果只是一场梦"),
                (2.88, "どれほど&よかった"),
                (2.88, "那该有多好"),
            ]
        );

        // Fallback to H5 when mobi.s fails and pic returns NO_PIC.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin2 = format!("http://{}", listener.local_addr().unwrap());
        let cover_url2 = format!("{origin2}/h5cover.png");
        let server2 = thread::spawn(move || {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut hdr = String::new();
                while reader
                    .read_line(&mut hdr)
                    .is_ok_and(|n| n > 0 && hdr != "\r\n" && hdr != "\n")
                {
                    hdr.clear();
                }
                let resp = if path.starts_with("/kw/pic") {
                    http_resp("200 OK", "text/plain", b"NO_PIC")
                } else if path.starts_with("/kw/mobi") {
                    http_resp("500 Internal Server Error", "text/plain", b"err")
                } else if path.starts_with("/kw/h5") {
                    let body = json!({
                        "data": {
                            "songinfo": {"pic": cover_url2},
                            "lrclist": [
                                {"time": "1.54", "lineLyric": "夢ならば"},
                                {"time": "2.88", "lineLyric": "如果只是一场梦"},
                                {"time": "2.88", "lineLyric": "どれほど"},
                                {"time": "6.88", "lineLyric": "那该有多好"},
                            ]
                        }
                    })
                    .to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                } else {
                    http_resp("200 OK", "image/png", &png)
                };
                stream.write_all(&resp).unwrap();
            }
        });
        let fallback = runtime()
            .block_on(fetch_details_with(
                &client(),
                &endpoints_for(&origin2),
                &track,
                Default::default(),
            ))
            .unwrap();
        server2.join().unwrap();
        assert_eq!(fallback.cover.unwrap().dimensions(), (4, 4));
        assert_eq!(fallback.lyrics.len(), 4);
        assert_eq!(fallback.lyrics[3].text, "那该有多好");
    }

    #[test]
    fn kg_fetches_privilege_cover_and_decodes_krc_with_translation() {
        let png = png_bytes(6, 6);
        let lang = BASE64.encode(
            json!({
                "content": [
                    {"type": 0, "lyricContent": [["yu me"]]},
                    {"type": 1, "lyricContent": [["如果", "只是一场梦"], ["那该有多好"]]}
                ]
            })
            .to_string(),
        );
        let krc = encode_kg_krc(&format!(
            "[id:$00000000]\n[language:{lang}]\n\
             [1540,1340]<0,500,0>夢<500,840,0>ならば\n\
             [2880,4000]<0,2000,0>どれほど&#38;よかった"
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let cover_tpl = format!("{origin}/stdmusic/{{size}}/cover.png");
        let server = thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut content_len = 0usize;
                let mut hdr = String::new();
                while reader
                    .read_line(&mut hdr)
                    .is_ok_and(|n| n > 0 && hdr != "\r\n" && hdr != "\n")
                {
                    if let Some((k, v)) = hdr.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                    hdr.clear();
                }
                let mut discard = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut discard);
                }
                let resp = if path.starts_with("/kg/privilege") {
                    let body = json!({
                        "data": [{"info": {"image": cover_tpl, "imgsize": [150, 480]}}]
                    })
                    .to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                } else if path == "/stdmusic/480/cover.png" {
                    http_resp("200 OK", "image/png", &png)
                } else if path.starts_with("/kg/lrc_search") {
                    assert!(path.contains("timelength=185000"));
                    let body = json!({
                        "candidates": [{"id": "100", "accesskey": "KEY", "krctype": 1, "contenttype": 0}]
                    })
                    .to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                } else {
                    assert!(path.contains("fmt=krc"));
                    let body = json!({"fmt": "krc", "content": krc}).to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                };
                stream.write_all(&resp).unwrap();
            }
        });
        let track = OnlineTrack::from_info(json!({
            "source": "kg", "name": "Lemon", "singer": "Artist", "songmid": 1002,
            "hash": "aabbcc", "_interval": 185, "types": [{"type": "128k", "hash": "aabbcc"}],
        }))
        .unwrap();
        let details = runtime()
            .block_on(fetch_details_with(
                &client(),
                &endpoints_for(&origin),
                &track,
                Default::default(),
            ))
            .unwrap();
        server.join().unwrap();
        assert_eq!(details.cover.unwrap().dimensions(), (6, 6));
        let texts: Vec<_> = details.lyrics.iter().map(|l| (l.time, l.text.as_str())).collect();
        assert_eq!(
            texts,
            vec![
                (1.54, "夢ならば"),
                (1.54, "如果只是一场梦"),
                (2.88, "どれほど&よかった"),
                (2.88, "那该有多好"),
            ]
        );
    }

    #[test]
    fn tx_wy_and_mg_fetch_covers_and_lyrics_with_fallbacks() {
        let png = png_bytes(5, 5);

        // 1. QQ Music (tx): albumMid cover + musicu PlayLyricInfo cleaning `//` placeholder trans.
        let png_tx = png.clone();
        let (origin_tx, server_tx) = start_server(2, move |path, _| {
            if path == "/tx/pic/T002R500x500M000albMid.jpg" {
                http_resp("200 OK", "image/png", &png_tx)
            } else if path == "/tx/musicu" {
                let body = json!({
                    "code": 0,
                    "req": {
                        "code": 0,
                        "data": {
                            "lyric": BASE64.encode("[00:00.00]词：测试\n[00:01.54]夢 &amp; 光"),
                            "trans": BASE64.encode("[00:00.00]//\n[00:01.54]梦与光"),
                        }
                    }
                })
                .to_string();
                http_resp("200 OK", "application/json", body.as_bytes())
            } else {
                http_resp("404 Not Found", "text/plain", b"")
            }
        });
        let tx_track = OnlineTrack::from_info(json!({
            "source": "tx", "name": "Song", "singer": "Artist", "songmid": "MID1",
            "strMediaMid": "MEDIA1", "albumMid": "albMid", "songId": 42,
            "types": [{"type": "128k"}],
        }))
        .unwrap();
        let tx_details = runtime()
            .block_on(fetch_details_with(
                &client(),
                &endpoints_for(&origin_tx),
                &tx_track,
                Default::default(),
            ))
            .unwrap();
        server_tx.join().unwrap();
        assert_eq!(tx_details.cover.unwrap().dimensions(), (5, 5));
        let tx_lines: Vec<_> = tx_details.lyrics.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(tx_lines, vec!["词：测试", "夢 & 光", "梦与光"]);

        // 2. NetEase (wy): missing img falls back to eapi song detail; yrc aligns ytlrc within 100ms.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_wy = format!("http://{}", listener.local_addr().unwrap());
        let wy_cover_url = format!("{origin_wy}/wy.png");
        let png_wy = png.clone();
        let server_wy = thread::spawn(move || {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut content_len = 0usize;
                let mut hdr = String::new();
                while reader
                    .read_line(&mut hdr)
                    .is_ok_and(|n| n > 0 && hdr != "\r\n" && hdr != "\n")
                {
                    if let Some((k, v)) = hdr.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                    hdr.clear();
                }
                let mut discard = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut discard);
                }
                let resp = if path == "/wy/detail" {
                    let body = json!({
                        "code": 200,
                        "songs": [{"al": {"picUrl": wy_cover_url}}]
                    })
                    .to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                } else if path == "/wy.png" {
                    http_resp("200 OK", "image/png", &png_wy)
                } else {
                    let body = json!({
                        "code": 200,
                        "yrc": {
                            "lyric": format!(
                                "{}\n[1540,1340](1540,500,0)夢(2040,840,0)ならば",
                                json!({"t": 0, "c": [{"tx": "作曲: "}, {"tx": "米津玄師"}]})
                            )
                        },
                        "ytlrc": {"lyric": "[00:01.520]如果只是一场梦"}
                    })
                    .to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                };
                stream.write_all(&resp).unwrap();
            }
        });
        let wy_track = OnlineTrack::from_info(json!({
            "source": "wy", "name": "Lemon", "singer": "米津玄師", "songmid": 536622304,
            "types": [{"type": "128k"}],
        }))
        .unwrap();
        let wy_details = runtime()
            .block_on(fetch_details_with(
                &client(),
                &endpoints_for(&origin_wy),
                &wy_track,
                Default::default(),
            ))
            .unwrap();
        server_wy.join().unwrap();
        assert_eq!(wy_details.cover.unwrap().dimensions(), (5, 5));
        let wy_lines: Vec<_> =
            wy_details.lyrics.iter().map(|l| (l.time, l.text.as_str())).collect();
        assert_eq!(
            wy_lines,
            vec![(0.0, "作曲: 米津玄師"), (1.54, "夢ならば"), (1.54, "如果只是一场梦")]
        );

        // 3. Migu (mg): resourceinfo fallback resolves both albumImgs cover and lrcUrl/trcUrl.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin_mg = format!("http://{}", listener.local_addr().unwrap());
        let mg_img = format!("{origin_mg}/mg.png");
        let mg_lrc = format!("{origin_mg}/1.lrc");
        let mg_trc = format!("{origin_mg}/1.trc");
        let server_mg = thread::spawn(move || {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut content_len = 0usize;
                let mut hdr = String::new();
                while reader
                    .read_line(&mut hdr)
                    .is_ok_and(|n| n > 0 && hdr != "\r\n" && hdr != "\n")
                {
                    if let Some((k, v)) = hdr.split_once(':')
                        && k.eq_ignore_ascii_case("content-length")
                    {
                        content_len = v.trim().parse().unwrap_or(0);
                    }
                    hdr.clear();
                }
                let mut discard = vec![0u8; content_len];
                if content_len > 0 {
                    let _ = reader.read_exact(&mut discard);
                }
                let resp = if path.starts_with("/mg/resource") {
                    let body = json!({
                        "code": "000000",
                        "resource": [{
                            "albumImgs": [{"imgSizeType": "03", "img": mg_img}],
                            "lrcUrl": mg_lrc,
                            "trcUrl": mg_trc,
                        }]
                    })
                    .to_string();
                    http_resp("200 OK", "application/json", body.as_bytes())
                } else if path == "/mg.png" {
                    http_resp("200 OK", "image/png", &png)
                } else if path == "/1.lrc" {
                    http_resp("200 OK", "text/plain", "[00:01.00]原文\r\n".as_bytes())
                } else {
                    http_resp("200 OK", "text/plain", "[00:01.00]译文\r\n".as_bytes())
                };
                stream.write_all(&resp).unwrap();
            }
        });
        let mg_track = OnlineTrack::from_info(json!({
            "source": "mg", "name": "晴天", "singer": "周杰伦", "songmid": "MG2",
            "copyrightId": "CP2", "types": [{"type": "128k"}],
        }))
        .unwrap();
        let mg_details = runtime()
            .block_on(fetch_details_with(
                &client(),
                &endpoints_for(&origin_mg),
                &mg_track,
                Default::default(),
            ))
            .unwrap();
        server_mg.join().unwrap();
        assert_eq!(mg_details.cover.unwrap().dimensions(), (5, 5));
        let mg_lines: Vec<_> =
            mg_details.lyrics.iter().map(|l| (l.time, l.text.as_str())).collect();
        assert_eq!(mg_lines, vec![(1.0, "原文"), (1.0, "译文")]);
    }

    #[test]
    fn inline_lrc_and_wy_colon_timestamps_work_and_non_http_cover_is_rejected() {
        let track = OnlineTrack::from_info(json!({
            "source": "wy", "name": "Song", "singer": "Artist", "songmid": 1,
            "img": "file:///etc/passwd",
            "lrc": "[00:01:54]Colon &#39;Line&#39;\n[00:12:345]Three-digit ms\n[100:02:05]Long song",
            "types": [{"type": "128k"}],
        }))
        .unwrap();
        let inline = inline_details(&track);
        assert_eq!(inline.lyrics.len(), 3);
        assert_eq!((inline.lyrics[0].time, inline.lyrics[0].text.as_str()), (1.54, "Colon 'Line'"));
        assert!((inline.lyrics[1].time - 12.345).abs() < 1e-3);
        assert_eq!(inline.lyrics[1].text, "Three-digit ms");
        assert!((inline.lyrics[2].time - 6002.05).abs() < 1e-2);
        assert!(as_http_url("file:///etc/passwd").is_none());

        // Header-only yrc must fall back to full lrc instead of returning credit lines alone.
        let wy_fallback = parse_wy_lyric_response(&json!({
            "yrc": {"lyric": json!({"t": 0, "c": [{"tx": "作词: 测试"}]}).to_string()},
            "lrc": {"lyric": "[00:05.00]Full LRC verse"},
        }));
        assert_eq!(wy_fallback.len(), 1);
        assert_eq!((wy_fallback[0].time, wy_fallback[0].text.as_str()), (5.0, "Full LRC verse"));

        // Huge timestamps / intervals must return None / 0 without arithmetic overflow.
        assert!(parse_time_ms("999999999999999999:00.00").is_none());
        assert_eq!(duration_ms_from_info(&json!({"interval": "999999999999999999:00"})), 0);
        assert_eq!(duration_ms_from_info(&json!({"_interval": u64::MAX})), 0);
    }
}
