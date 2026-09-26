use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use worker::{Request, Response, Result, RouteContext};

use crate::auth;
use crate::error::AppError;
use crate::rate_limit::{enforce_read_rate_limit, enforce_write_rate_limit};

/// Tenants allowed to publish playlist pages.
const SHARING_APPS: &[&str] = &["psywave"];
const PUBLIC_BASE_URL: &str = "https://psywave.midgarcorp.cc";
const APP_STORE_ID: &str = "6727000827";
const PROVIDER_TOKEN: &str = "120194828";
const APP_ICON_URL: &str = "https://is1-ssl.mzstatic.com/image/thumb/Purple221/v4/72/3d/af/723daf01-15fa-2487-876c-16fa0a6fcf9e/AppIcon-0-0-1x_U007epad-0-1-85-220.png/512x512bb.jpg";
const MAX_SONGS: usize = 100;
const MAX_SHARES_PER_DAY: i64 = 100;
const ID_LENGTH: usize = 10;

#[derive(Debug, Deserialize)]
pub struct ShareRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub mood: String,
    #[serde(default)]
    pub genre: String,
    pub storefront: Option<String>,
    pub songs: Vec<ShareSongRequest>,
}

#[derive(Debug, Deserialize)]
pub struct ShareSongRequest {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    #[serde(rename = "appleMusicID")]
    pub apple_music_id: Option<String>,
    #[serde(rename = "artworkURL")]
    pub artwork_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedPlaylist {
    pub name: String,
    pub description: String,
    pub mood: String,
    pub genre: String,
    pub storefront: Option<String>,
    pub songs: Vec<SharedSong>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SharedSong {
    pub title: String,
    pub artist: String,
    pub album: Option<String>,
    #[serde(rename = "appleMusicID")]
    pub apple_music_id: Option<String>,
    #[serde(rename = "artworkURL")]
    pub artwork_url: Option<String>,
}

#[derive(Serialize)]
struct SharedPlaylistResponse<'a> {
    id: &'a str,
    url: String,
    #[serde(flatten)]
    playlist: &'a SharedPlaylist,
}

/// POST /v1/playlists/share — publishes a playlist as a public page and returns
/// its address. Only text fields and Apple Music identifiers are kept; every
/// field is length-capped, stripped of control characters, and rendered
/// escaped, and artwork is accepted only from Apple's own image CDN.
pub async fn share(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    match share_inner(req, ctx).await {
        Ok(response) => Ok(response),
        Err(e) => e.to_response(),
    }
}

async fn share_inner(mut req: Request, ctx: RouteContext<()>) -> std::result::Result<Response, AppError> {
    let db = ctx.env.d1("DB")?;
    let auth = auth::authenticate(&req, &db).await?;
    if !SHARING_APPS.contains(&auth.app_id.as_str()) {
        return Err(AppError::Forbidden("Sharing is not available for this app".to_string()));
    }
    enforce_write_rate_limit(&ctx.env, &auth.app_id, &auth.user_id, "playlists.share").await?;

    let body: ShareRequest = req
        .json()
        .await
        .map_err(|_| AppError::BadRequest("Invalid request body".to_string()))?;
    let playlist = sanitize(body).map_err(AppError::BadRequest)?;

    let since = (Utc::now() - Duration::hours(24)).to_rfc3339();
    let recent = db
        .prepare("SELECT COUNT(*) AS n FROM shared_playlists WHERE app_id = ? AND user_id = ? AND created_at >= ?")
        .bind(&[auth.app_id.clone().into(), auth.user_id.clone().into(), since.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .and_then(|v| v.get("n").and_then(|n| n.as_i64()))
        .unwrap_or(0);
    if recent >= MAX_SHARES_PER_DAY {
        return Err(AppError::RateLimitExceeded);
    }

    let payload = serde_json::to_string(&playlist)
        .map_err(|e| AppError::InternalError(format!("share payload: {}", e)))?;
    let now = Utc::now().to_rfc3339();
    for _ in 0..3 {
        let id = random_id()?;
        let inserted = db
            .prepare(
                "INSERT OR IGNORE INTO shared_playlists (id, app_id, user_id, payload, created_at)
                 VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&[
                id.clone().into(),
                auth.app_id.clone().into(),
                auth.user_id.clone().into(),
                payload.clone().into(),
                now.clone().into(),
            ])?
            .run()
            .await?;
        if inserted.meta()?.and_then(|m| m.changes).unwrap_or(0) > 0 {
            return Ok(Response::from_json(&SharedPlaylistResponse {
                id: &id,
                url: page_url(&id),
                playlist: &playlist,
            })?
            .with_status(201));
        }
    }
    Err(AppError::InternalError("Could not allocate a playlist id".to_string()))
}

/// GET /v1/playlists/:id — the playlist as JSON, for the app to open a link.
pub async fn fetch(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let id = ctx.param("id").cloned().unwrap_or_default();
    match load(&req, &ctx, &id).await {
        Ok(playlist) => Response::from_json(&SharedPlaylistResponse { id: &id, url: page_url(&id), playlist: &playlist }),
        Err(e) => e.to_response(),
    }
}

/// GET /p/:id — the public page.
pub async fn page(req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let id = ctx.param("id").cloned().unwrap_or_default();
    let language = preferred_language(req.headers().get("Accept-Language").ok().flatten().as_deref());
    let (status, body) = match load(&req, &ctx, &id).await {
        Ok(playlist) => (200, render_page(&id, &playlist, language)),
        Err(AppError::RateLimitExceeded) => (429, render_missing(language)),
        Err(_) => (404, render_missing(language)),
    };
    let mut response = Response::ok(body)?.with_status(status);
    response.headers_mut().set("Content-Type", "text/html; charset=utf-8")?;
    response.headers_mut().set("Cache-Control", "public, max-age=300")?;
    response.headers_mut().set("X-Robots-Tag", "noindex")?;
    response
        .headers_mut()
        .set("Content-Security-Policy", "default-src 'none'; img-src https://*.mzstatic.com; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'")?;
    Ok(response)
}

/// `/` on the Psywave host goes to the App Store; the API host keeps its index.
pub fn is_public_host(req: &Request) -> bool {
    req.url()
        .ok()
        .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case("psywave.midgarcorp.cc")))
        .unwrap_or(false)
}

pub fn app_store_url(campaign: &str) -> String {
    format!(
        "https://apps.apple.com/app/id{}?pt={}&ct={}&mt=8",
        APP_STORE_ID, PROVIDER_TOKEN, campaign
    )
}

async fn load(req: &Request, ctx: &RouteContext<()>, id: &str) -> std::result::Result<SharedPlaylist, AppError> {
    if !is_valid_id(id) {
        return Err(AppError::NotFound("Playlist not found".to_string()));
    }
    let ip = req
        .headers()
        .get("CF-Connecting-IP")
        .ok()
        .flatten()
        .unwrap_or_else(|| "unknown".to_string());
    enforce_read_rate_limit(&ctx.env, "psywave", &format!("ip:{}", ip), "playlists.read").await?;
    let db = ctx.env.d1("DB")?;
    let row = db
        .prepare("SELECT payload FROM shared_playlists WHERE id = ?")
        .bind(&[id.into()])?
        .first::<serde_json::Value>(None)
        .await?
        .ok_or_else(|| AppError::NotFound("Playlist not found".to_string()))?;
    let payload = row.get("payload").and_then(|p| p.as_str()).unwrap_or("");
    serde_json::from_str(payload).map_err(|e| AppError::InternalError(format!("stored playlist: {}", e)))
}

fn page_url(id: &str) -> String {
    format!("{}/p/{}", PUBLIC_BASE_URL, id)
}

fn random_id() -> std::result::Result<String, AppError> {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
    let mut bytes = [0u8; ID_LENGTH];
    getrandom::getrandom(&mut bytes).map_err(|e| AppError::InternalError(format!("rng: {}", e)))?;
    Ok(bytes.iter().map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char).collect())
}

pub fn is_valid_id(id: &str) -> bool {
    (6..=24).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric())
}

/// Trims, strips control characters and caps a field at `max` characters.
fn clean(text: &str, max: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max)
        .collect()
}

fn clean_optional(text: Option<&str>, max: usize) -> Option<String> {
    text.map(|t| clean(t, max)).filter(|t| !t.is_empty())
}

pub fn sanitize(request: ShareRequest) -> std::result::Result<SharedPlaylist, String> {
    let name = clean(&request.name, 120);
    if name.is_empty() {
        return Err("name is required".to_string());
    }
    let songs: Vec<SharedSong> = request
        .songs
        .iter()
        .filter_map(|song| {
            let title = clean(&song.title, 200);
            let artist = clean(&song.artist, 200);
            if title.is_empty() || artist.is_empty() {
                return None;
            }
            Some(SharedSong {
                title,
                artist,
                album: clean_optional(song.album.as_deref(), 200),
                apple_music_id: song
                    .apple_music_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|id| (1..=20).contains(&id.len()) && id.chars().all(|c| c.is_ascii_digit()))
                    .map(str::to_string),
                artwork_url: song.artwork_url.as_deref().and_then(safe_artwork_url),
            })
        })
        .take(MAX_SONGS)
        .collect();
    if songs.is_empty() {
        return Err("at least one song is required".to_string());
    }
    let storefront = request
        .storefront
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_lowercase()));
    Ok(SharedPlaylist {
        name,
        description: clean(&request.description, 400),
        mood: clean(&request.mood, 80),
        genre: clean(&request.genre, 60),
        storefront,
        songs,
    })
}

fn safe_artwork_url(raw: &str) -> Option<String> {
    let url = url::Url::parse(raw.trim()).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    if url.scheme() != "https" || !host.ends_with(".mzstatic.com") || raw.len() > 400 {
        return None;
    }
    Some(url.to_string())
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PageLanguage {
    En, De, Es, Fr, It, Ja, Ko, Pl, PtBr, ZhHans, ZhHant,
}

struct PageText {
    songs: &'static str,
    open_in_app: &'static str,
    get_app: &'static str,
    made_with: &'static str,
    pitch: &'static str,
    missing: &'static str,
    report: &'static str,
}

fn text(language: PageLanguage) -> PageText {
    match language {
        PageLanguage::En => PageText { songs: "songs", open_in_app: "Open in Psywave", get_app: "Get Psywave — free", made_with: "Made with Psywave", pitch: "Turn any vibe or photo into a playlist of real songs.", missing: "This playlist is no longer available.", report: "Report this playlist" },
        PageLanguage::De => PageText { songs: "Songs", open_in_app: "In Psywave öffnen", get_app: "Psywave laden – gratis", made_with: "Erstellt mit Psywave", pitch: "Mach aus jeder Stimmung und jedem Foto eine Playlist mit echten Songs.", missing: "Diese Playlist ist nicht mehr verfügbar.", report: "Playlist melden" },
        PageLanguage::Es => PageText { songs: "canciones", open_in_app: "Abrir en Psywave", get_app: "Consigue Psywave gratis", made_with: "Hecho con Psywave", pitch: "Convierte cualquier ambiente o foto en una playlist de canciones reales.", missing: "Esta playlist ya no está disponible.", report: "Denunciar esta playlist" },
        PageLanguage::Fr => PageText { songs: "titres", open_in_app: "Ouvrir dans Psywave", get_app: "Obtenir Psywave gratuitement", made_with: "Créé avec Psywave", pitch: "Transformez une ambiance ou une photo en playlist de vrais titres.", missing: "Cette playlist n'est plus disponible.", report: "Signaler cette playlist" },
        PageLanguage::It => PageText { songs: "brani", open_in_app: "Apri in Psywave", get_app: "Scarica Psywave gratis", made_with: "Creata con Psywave", pitch: "Trasforma qualsiasi atmosfera o foto in una playlist di brani veri.", missing: "Questa playlist non è più disponibile.", report: "Segnala questa playlist" },
        PageLanguage::Ja => PageText { songs: "曲", open_in_app: "Psywaveで開く", get_app: "Psywaveを無料で入手", made_with: "Psywaveで作成", pitch: "気分や写真から、本物の曲でプレイリストを作成。", missing: "このプレイリストは公開が終了しました。", report: "このプレイリストを報告" },
        PageLanguage::Ko => PageText { songs: "곡", open_in_app: "Psywave에서 열기", get_app: "Psywave 무료로 받기", made_with: "Psywave로 만든 플레이리스트", pitch: "어떤 기분이나 사진도 실제 노래로 채운 플레이리스트로.", missing: "더 이상 볼 수 없는 플레이리스트입니다.", report: "이 플레이리스트 신고" },
        PageLanguage::Pl => PageText { songs: "utworów", open_in_app: "Otwórz w Psywave", get_app: "Pobierz Psywave za darmo", made_with: "Stworzone w Psywave", pitch: "Zamień dowolny nastrój lub zdjęcie w playlistę prawdziwych utworów.", missing: "Ta playlista nie jest już dostępna.", report: "Zgłoś tę playlistę" },
        PageLanguage::PtBr => PageText { songs: "músicas", open_in_app: "Abrir no Psywave", get_app: "Baixe o Psywave grátis", made_with: "Feita com o Psywave", pitch: "Transforme qualquer vibe ou foto em uma playlist de músicas de verdade.", missing: "Esta playlist não está mais disponível.", report: "Denunciar esta playlist" },
        PageLanguage::ZhHans => PageText { songs: "首", open_in_app: "在 Psywave 中打开", get_app: "免费获取 Psywave", made_with: "由 Psywave 生成", pitch: "把任何心情或照片变成真实歌曲组成的歌单。", missing: "此歌单已不再可用。", report: "举报此歌单" },
        PageLanguage::ZhHant => PageText { songs: "首", open_in_app: "在 Psywave 中開啟", get_app: "免費取得 Psywave", made_with: "由 Psywave 產生", pitch: "把任何心情或照片變成真實歌曲組成的歌單。", missing: "此歌單已無法使用。", report: "檢舉此歌單" },
    }
}

/// Picks the page language from the first `Accept-Language` entry the app is
/// translated into; Chinese follows the script, with Taiwan and Hong Kong
/// reading Traditional.
pub fn preferred_language(header: Option<&str>) -> PageLanguage {
    let Some(header) = header else { return PageLanguage::En };
    for part in header.split(',') {
        let tag = part.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        let primary = tag.split('-').next().unwrap_or("");
        let language = match primary {
            "en" => Some(PageLanguage::En),
            "de" => Some(PageLanguage::De),
            "es" => Some(PageLanguage::Es),
            "fr" => Some(PageLanguage::Fr),
            "it" => Some(PageLanguage::It),
            "ja" => Some(PageLanguage::Ja),
            "ko" => Some(PageLanguage::Ko),
            "pl" => Some(PageLanguage::Pl),
            "pt" => Some(PageLanguage::PtBr),
            "zh" => Some(
                if tag.contains("hant") || tag.ends_with("-tw") || tag.ends_with("-hk") || tag.ends_with("-mo") {
                    PageLanguage::ZhHant
                } else {
                    PageLanguage::ZhHans
                },
            ),
            _ => None,
        };
        if let Some(language) = language {
            return language;
        }
    }
    PageLanguage::En
}

fn html_lang(language: PageLanguage) -> &'static str {
    match language {
        PageLanguage::En => "en",
        PageLanguage::De => "de",
        PageLanguage::Es => "es",
        PageLanguage::Fr => "fr",
        PageLanguage::It => "it",
        PageLanguage::Ja => "ja",
        PageLanguage::Ko => "ko",
        PageLanguage::Pl => "pl",
        PageLanguage::PtBr => "pt-BR",
        PageLanguage::ZhHans => "zh-Hans",
        PageLanguage::ZhHant => "zh-Hant",
    }
}

const STYLE: &str = "*{box-sizing:border-box}body{margin:0;background:#0A0612;color:#F5F2FF;font-family:-apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;-webkit-font-smoothing:antialiased}.glow{background:radial-gradient(120% 60% at 20% 0%,rgba(143,89,247,.45),rgba(240,86,142,.14) 45%,transparent 75%)}.wrap{max-width:680px;margin:0 auto;padding:28px 20px 48px}.brand{font-size:13px;letter-spacing:.3em;font-weight:700;color:#C9B3FF;text-decoration:none}h1{font-size:34px;line-height:1.1;margin:18px 0 10px;font-weight:800}.desc{color:rgba(255,255,255,.66);font-size:16px;line-height:1.5;margin:0 0 14px}.chips{display:flex;gap:8px;flex-wrap:wrap;margin-bottom:22px}.chip{background:rgba(143,89,247,.28);border-radius:999px;padding:6px 12px;font-size:13px;font-weight:600}.cta{display:flex;gap:10px;flex-wrap:wrap;margin-bottom:26px}.btn{flex:1 1 200px;text-align:center;border-radius:16px;padding:14px 16px;font-weight:700;font-size:16px;text-decoration:none;color:#fff}.primary{background:linear-gradient(90deg,#8F59F7,#F0568E)}.secondary{background:#1E1633;border:1px solid rgba(255,255,255,.12)}ol{list-style:none;margin:0;padding:0}li{display:flex;align-items:center;gap:12px;padding:10px 0;border-bottom:1px solid rgba(255,255,255,.06)}.art{width:52px;height:52px;border-radius:10px;background:#1E1633;flex:0 0 52px;object-fit:cover}.meta{flex:1;min-width:0}.t{font-weight:600;font-size:15px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.a{color:rgba(255,255,255,.6);font-size:13px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}.links{display:flex;gap:6px}.links a{font-size:12px;font-weight:700;color:#fff;text-decoration:none;border-radius:8px;padding:6px 8px;background:#1E1633}.links .am{background:#FA2D48}.foot{margin-top:30px;text-align:center;color:rgba(255,255,255,.55);font-size:14px}.foot a{color:#C9B3FF}.foot .report{display:inline-block;margin-top:14px;font-size:12px;color:rgba(255,255,255,.4)}@media(max-width:520px){h1{font-size:28px}li{flex-wrap:wrap}.meta{flex:1 1 calc(100% - 64px)}.links{width:100%;padding-left:64px;margin-top:-4px}}";

pub fn render_page(id: &str, playlist: &SharedPlaylist, language: PageLanguage) -> String {
    let t = text(language);
    let storefront = playlist.storefront.as_deref().unwrap_or("us");
    let image = playlist
        .songs
        .iter()
        .find_map(|s| s.artwork_url.clone())
        .unwrap_or_else(|| APP_ICON_URL.to_string());
    let summary = format!(
        "{} {}{} · {}",
        playlist.songs.len(),
        t.songs,
        if playlist.mood.is_empty() { String::new() } else { format!(" · {}", playlist.mood) },
        t.made_with
    );

    let mut chips = format!(r#"<span class="chip">♪ {} {}</span>"#, playlist.songs.len(), escape(t.songs));
    for label in [&playlist.mood, &playlist.genre] {
        if !label.is_empty() {
            chips.push_str(&format!(r#"<span class="chip">{}</span>"#, escape(label)));
        }
    }

    let mut rows = String::new();
    for song in &playlist.songs {
        let query = urlencoding::encode(&format!("{} {}", song.title, song.artist)).into_owned();
        let artwork = match &song.artwork_url {
            Some(url) => format!(r#"<img class="art" src="{}" alt="" loading="lazy">"#, escape(url)),
            None => r#"<div class="art"></div>"#.to_string(),
        };
        let apple = match &song.apple_music_id {
            Some(song_id) => format!(
                r#"<a class="am" href="https://music.apple.com/{}/song/{}">Apple Music</a>"#,
                storefront, song_id
            ),
            None => format!(r#"<a class="am" href="https://music.apple.com/{}/search?term={}">Apple Music</a>"#, storefront, query),
        };
        rows.push_str(&format!(
            r#"<li>{artwork}<div class="meta"><div class="t">{title}</div><div class="a">{artist}</div></div><div class="links">{apple}<a href="https://open.spotify.com/search/{query}">Spotify</a><a href="https://music.youtube.com/search?q={query}">YouTube</a></div></li>"#,
            artwork = artwork,
            title = escape(&song.title),
            artist = escape(&song.artist),
            apple = apple,
            query = query,
        ));
    }

    let description = if playlist.description.is_empty() {
        String::new()
    } else {
        format!(r#"<p class="desc">{}</p>"#, escape(&playlist.description))
    };

    format!(
        r#"<!DOCTYPE html><html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>{title} · Psywave</title><meta name="robots" content="noindex"><meta name="description" content="{summary}"><meta name="apple-itunes-app" content="app-id={app_id}, app-argument=psywave://p/{id}"><meta property="og:type" content="music.playlist"><meta property="og:title" content="{title}"><meta property="og:description" content="{summary}"><meta property="og:image" content="{image}"><meta property="og:url" content="{url}"><meta name="twitter:card" content="summary"><style>{style}</style></head><body><div class="glow"><div class="wrap"><a class="brand" href="{store}">PSYWAVE</a><h1>{title}</h1>{description}<div class="chips">{chips}</div><div class="cta"><a class="btn primary" href="psywave://p/{id}">{open}</a><a class="btn secondary" href="{store}">{get}</a></div><ol>{rows}</ol><p class="foot">{pitch}<br><a href="{store}">{made_with}</a><br><a class="report" href="mailto:support@midgarcorp.cc?subject=Report%20playlist%20{id}">{report}</a></p></div></div></body></html>"#,
        lang = html_lang(language),
        title = escape(&playlist.name),
        summary = escape(&summary),
        app_id = APP_STORE_ID,
        id = id,
        image = escape(&image),
        url = escape(&page_url(id)),
        style = STYLE,
        store = escape(&app_store_url("share-page")),
        description = description,
        chips = chips,
        open = escape(t.open_in_app),
        get = escape(t.get_app),
        rows = rows,
        pitch = escape(t.pitch),
        made_with = escape(t.made_with),
        report = escape(t.report),
    )
}

fn render_missing(language: PageLanguage) -> String {
    let t = text(language);
    format!(
        r#"<!DOCTYPE html><html lang="{lang}"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>Psywave</title><meta name="robots" content="noindex"><meta name="apple-itunes-app" content="app-id={app_id}"><style>{style}</style></head><body><div class="glow"><div class="wrap"><a class="brand" href="{store}">PSYWAVE</a><h1>{missing}</h1><p class="desc">{pitch}</p><div class="cta"><a class="btn primary" href="{store}">{get}</a></div></div></div></body></html>"#,
        lang = html_lang(language),
        app_id = APP_STORE_ID,
        style = STYLE,
        store = escape(&app_store_url("share-missing")),
        missing = escape(t.missing),
        pitch = escape(t.pitch),
        get = escape(t.get_app),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(name: &str, songs: Vec<ShareSongRequest>) -> ShareRequest {
        ShareRequest {
            name: name.to_string(),
            description: "  Neon\u{0007} nights  ".to_string(),
            mood: "Nocturnal · Euphoric".to_string(),
            genre: "Synthwave".to_string(),
            storefront: Some("JP".to_string()),
            songs,
        }
    }

    fn song(title: &str, id: Option<&str>, art: Option<&str>) -> ShareSongRequest {
        ShareSongRequest {
            title: title.to_string(),
            artist: "M83".to_string(),
            album: None,
            apple_music_id: id.map(str::to_string),
            artwork_url: art.map(str::to_string),
        }
    }

    #[test]
    fn sanitize_cleans_fields_and_rejects_foreign_urls() {
        let playlist = sanitize(request(
            "Midnight <Drive>",
            vec![
                song("Midnight City", Some("828259377"), Some("https://is1-ssl.mzstatic.com/image/a/600x600bb.jpg")),
                song("Evil", Some("12a"), Some("https://evil.example.com/x.jpg")),
                song("   ", None, None),
            ],
        ))
        .unwrap();
        assert_eq!(playlist.description, "Neon nights");
        assert_eq!(playlist.storefront.as_deref(), Some("jp"));
        assert_eq!(playlist.songs.len(), 2);
        assert_eq!(playlist.songs[0].apple_music_id.as_deref(), Some("828259377"));
        assert!(playlist.songs[0].artwork_url.is_some());
        assert_eq!(playlist.songs[1].apple_music_id, None);
        assert_eq!(playlist.songs[1].artwork_url, None);
    }

    #[test]
    fn sanitize_requires_a_name_and_a_song() {
        assert!(sanitize(request("  ", vec![song("A", None, None)])).is_err());
        assert!(sanitize(request("Name", vec![])).is_err());
    }

    #[test]
    fn page_escapes_everything_user_supplied() {
        let playlist = sanitize(request(
            "<script>alert(1)</script>",
            vec![song("\"><img src=x onerror=alert(1)>", Some("828259377"), None)],
        ))
        .unwrap();
        let html = render_page("AbCdEf2345", &playlist, PageLanguage::En);
        assert!(!html.contains("<script>alert"));
        assert!(!html.contains("<img src=x"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("https://music.apple.com/jp/song/828259377"));
        assert!(html.contains("app-argument=psywave://p/AbCdEf2345"));
        assert!(html.contains("mailto:support@midgarcorp.cc?subject=Report%20playlist%20AbCdEf2345"));
    }

    #[test]
    fn language_follows_accept_language() {
        assert_eq!(preferred_language(Some("ja-JP,ja;q=0.9,en;q=0.8")), PageLanguage::Ja);
        assert_eq!(preferred_language(Some("zh-TW,zh;q=0.9")), PageLanguage::ZhHant);
        assert_eq!(preferred_language(Some("zh-CN")), PageLanguage::ZhHans);
        assert_eq!(preferred_language(Some("pt-BR,pt;q=0.9")), PageLanguage::PtBr);
        assert_eq!(preferred_language(Some("fi-FI,sv;q=0.8")), PageLanguage::En);
        assert_eq!(preferred_language(None), PageLanguage::En);
    }

    #[test]
    fn ids_are_validated() {
        assert!(is_valid_id("AbCdEf2345"));
        assert!(!is_valid_id("../../etc"));
        assert!(!is_valid_id("abc"));
    }
}
