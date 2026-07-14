use worker::{Request, Response, Result, RouteContext};

pub fn privacy_handler(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let app = ctx.param("app").map(|s| s.as_str()).unwrap_or("");
    Response::ok(render(app)).map(|mut r| {
        let _ = r.headers_mut().set("Content-Type", "text/html; charset=utf-8");
        r
    })
}

struct AppPrivacy {
    name: &'static str,
    ai: &'static str,
    uses_location: bool,
    voice_consent: bool,
}

fn config(app: &str) -> AppPrivacy {
    match app {
        "pixie" => AppPrivacy {
            name: "PixiePocket",
            ai: "the text prompts you enter, and any photos you provide for editing, are sent to OpenAI and Google to generate or edit images",
            uses_location: false,
            voice_consent: false,
        },
        "dreameater" => AppPrivacy {
            name: "Dream Eater",
            ai: "the dream you write is sent to Google Gemini to generate an interpretation, and an illustrative image is generated from it",
            uses_location: true,
            voice_consent: false,
        },
        "doublekick" => AppPrivacy {
            name: "Double Kick",
            ai: "photos of the menus you scan are sent to Google Gemini to read and translate the items",
            uses_location: false,
            voice_consent: false,
        },
        "psywave" => AppPrivacy {
            name: "Psywave",
            ai: "the description or photo you provide is sent to Google Gemini to generate a playlist",
            uses_location: false,
            voice_consent: false,
        },
        "psybeam" => AppPrivacy {
            name: "Psybeam",
            ai: "your speech is streamed directly from your device to OpenAI's real-time translation service over an encrypted connection and spoken back to you; the audio is processed only to translate, is not stored on our servers, and the conversation transcript stays on your device",
            uses_location: true,
            voice_consent: true,
        },
        "payday" => AppPrivacy {
            name: "Pay Day",
            ai: "only when you tap an AI action to draft line items from a photo or a description, the content you provide (that photo or text, plus the invoice currency) is sent over an encrypted connection to our backend, operated by Midgar Oy, which forwards it to OpenAI (OpenAI, L.L.C.) solely to generate the draft you requested. Nothing is sent until you grant permission in the app's AI consent screen, which you can withdraw at any time in Settings. Your saved invoices and client list are not sent",
            uses_location: false,
            voice_consent: false,
        },
        "livingdex" => AppPrivacy {
            name: "Living Dex",
            ai: "when the app can't identify a subject on your device, the photo you capture is sent to our AI providers (Anthropic and Google) to identify the organism and write its entry; on-device identification and narration happen locally and send nothing. We also query public biodiversity sources (GBIF, Wikipedia) by species name, not by your photo",
            uses_location: true,
            voice_consent: false,
        },
        _ => AppPrivacy {
            name: "This app",
            ai: "the content you submit is sent to our AI providers (OpenAI and Google) only to produce the result you requested",
            uses_location: false,
            voice_consent: false,
        },
    }
}

fn einvoicing_section(app: &str) -> &'static str {
    if app == "payday" {
        r#"<h2>Invoice and client data</h2>
<p>The invoices and estimates you create — including your business details and your clients' names, addresses, VAT identifiers, and the amounts — are stored on your device. Two optional actions send data off your device, only when you start them:</p>
<ul>
<li><strong>VAT validation:</strong> a client's VAT number is sent to the EU VIES service to confirm it is valid.</li>
<li><strong>Peppol delivery:</strong> the complete electronic invoice (an EN 16931 document with the buyer and seller details and the financial breakdown) is transmitted through our certified Peppol access-point provider to your client's accounting system.</li>
</ul>
<p>Because invoices necessarily contain information about your clients (third parties), that information is processed and transmitted as above solely to deliver the document or check the number you requested. You are responsible for having a lawful basis to invoice your clients. You can always produce and share an invoice as a PDF without using either action.</p>"#
    } else {
        ""
    }
}

fn contact_link(app: &str) -> &'static str {
    if app == "payday" {
        r#"<a href="mailto:support@midgarcorp.cc">support@midgarcorp.cc</a>"#
    } else {
        r#"<a href="https://x.com/prblemslver">@prblemslver</a>"#
    }
}

fn on_device_content(app: &str) -> &'static str {
    match app {
        "payday" => "your invoices, estimates, clients, logo, and settings",
        "livingdex" => "your captured photos, your species collection (your Dex), and your progress",
        _ => "results, history, and transcripts",
    }
}

fn render(app: &str) -> String {
    if app == "flaccy" {
        return render_flaccy();
    }
    let c = config(app);
    let location = if app == "livingdex" {
        "<h2>Location</h2><p>If you grant location access (While Using the App only), your approximate location tags each sighting on your device and is used to gauge how rare a species is where you are. For species of conservation concern, locations are coarsened so they cannot be used to find a vulnerable organism, and your precise location is never shared with other players.</p>"
    } else if c.uses_location {
        "<h2>Location</h2><p>If you grant location access, your approximate location is used on your device to improve the experience (such as suggesting a nearby language) and is not sent to our servers.</p>"
    } else {
        ""
    };
    let consent = if c.voice_consent {
        " Where the app shows a cloud-AI consent control, you can withdraw it at any time in Settings; translation is unavailable until consent is granted."
    } else {
        ""
    };
    let einvoicing = einvoicing_section(app);
    let contact = contact_link(app);
    let on_device = on_device_content(app);
    let purchases = if app == "payday" {
        "Subscriptions and credit packs are sold through Apple In-App Purchase and validated via RevenueCat. We receive purchase records (which product and a transaction identifier) to unlock features or credit your balance. We never receive your payment-card details."
    } else if app == "livingdex" {
        "Living Dex Pro (an auto-renewing subscription) is sold through Apple In-App Purchase and validated via RevenueCat. We receive purchase records (which product and a transaction identifier) to unlock Pro features. We never receive your payment-card details."
    } else {
        "Credit packs are sold through Apple In-App Purchase and validated via RevenueCat. We receive purchase records (which pack and a transaction identifier) to credit your balance. We never receive your payment-card details."
    };
    let deletion = if app == "payday" || app == "psybeam" {
        "You can delete your account from within the app at any time (in Settings), which removes your server-side identity and credit ledger; deleting the app removes any remaining on-device data."
    } else if app == "livingdex" {
        "Deleting the app removes your on-device collection and photos. Your anonymous credit identity holds no personal data; contact us to erase it."
    } else {
        "You can delete the app at any time to remove on-device data."
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name} — Privacy Policy</title>
<style>body{{font-family:-apple-system,BlinkMacSystemFont,system-ui,sans-serif;max-width:680px;margin:40px auto;padding:0 20px;line-height:1.6;color:#1c1c1e}}h1{{font-size:28px}}h2{{font-size:18px;margin-top:28px}}a{{color:#06c}}@media(prefers-color-scheme:dark){{body{{background:#000;color:#e5e5ea}}a{{color:#4da3ff}}}}</style>
</head><body>
<h1>{name} Privacy Policy</h1>
<p><em>Last updated: June 2026</em></p>
<p>{name} is designed to collect as little as possible. This policy explains what is processed and why.</p>
<h2>AI processing</h2>
<p>To provide the app's core feature, {ai}. This data is processed only to produce your result and is not used to train AI models. Our AI providers handle it under their own privacy and security commitments that protect it to a standard comparable to this policy, and are permitted to use it only to return your result.</p>
<h2>Identity</h2>
<p>{name} works without an account. On first launch we create an anonymous, device-based identity used only to track your credit balance. You may optionally Sign in with Apple to sync your balance across devices, in which case we receive only the identifier Apple provides.</p>
<h2>Purchases</h2>
<p>{purchases}</p>
{einvoicing}
<h2>On-device data</h2>
<p>Content you create in the app — such as {on_device} — is stored on your device and is not uploaded to us.</p>
{location}
<h2>What we don't do</h2>
<p>We do not sell your data, show advertising, or use third-party tracking or advertising identifiers.</p>
<h2>Your choices</h2>
<p>{deletion}{consent} For any questions, contact {contact}.</p>
</body></html>"#,
        name = c.name,
        ai = c.ai,
        purchases = purchases,
        einvoicing = einvoicing,
        on_device = on_device,
        location = location,
        consent = consent,
        deletion = deletion,
        contact = contact,
    )
}

fn render_flaccy() -> String {
    let body = r#"<h1>Flaccy Privacy Policy</h1>
<p><em>Last updated: July 2026</em></p>
<p>Flaccy is designed to collect as little as possible. This policy explains what is processed and why.</p>
<h2>On-device data</h2>
<p>Flaccy works without an account — there is no sign-up, no analytics, no advertising, and no tracking. Your music library, playlists, and play history are stored on your device and are not uploaded to us.</p>
<h2>Last.fm (optional)</h2>
<p>If you connect a Last.fm account, you authenticate directly with Last.fm; we never see your Last.fm password. While connected, the app sends scrobbles (track, artist, album, and timestamp) and now-playing updates to Last.fm, and fetches your charts, loved tracks, and artist information from it. That data is handled under <a href="https://www.last.fm/legal/privacy">Last.fm's privacy policy</a>. You can disconnect at any time in Settings.</p>
<h2>AI metadata cleanup</h2>
<p>When an imported file lacks recognizable metadata, its file and folder names — never the audio content — are sent to the Groq API, where a language model identifies the artist, album, and track. No personal identifiers are attached, and per Groq's data policy this data is not used to train AI models.</p>
<h2>Other service lookups</h2>
<p>To enrich your library, the app fetches lyrics from LRCLIB, artwork and artist images from Apple Music/iTunes, MusicBrainz, and Last.fm, and share links from Songlink. These queries contain only track, artist, or album names.</p>
<h2>What we don't do</h2>
<p>We do not sell your data, show advertising, or use third-party tracking or advertising identifiers. No data leaves your device except the service calls described above.</p>
<h2>Your choices</h2>
<p>You can delete the app at any time to remove on-device data. For any questions, contact <a href="https://x.com/prblemslver">@prblemslver</a>.</p>"#;
    page("Flaccy", "Privacy Policy", body)
}

pub fn payday_landing(_req: Request, _ctx: RouteContext<()>) -> Result<Response> {
    let html = r###"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Pay Day: E-Invoice &amp; Peppol — invoices your tax authority can read</title>
<meta name="description" content="Pay Day is a native iOS invoice app for EU freelancers and small businesses, built for mandatory e-invoicing. Free unlimited invoices with a real EN 16931 VAT engine; Pro adds Factur-X / ZUGFeRD e-invoices and Peppol delivery.">
<style>
  :root{
    --bg:#ffffff;
    --surface:#f5f3ee;
    --surface-2:#ece8df;
    --ink:#14140f;
    --ink-soft:#4a4740;
    --line:#e2ddd1;
    --gold:#D99E1A;
    --gold-deep:#8a5f00;
    --near-black:#111008;
    --on-dark:#f4f0e6;
    --on-dark-soft:#b7b1a2;
    --dark-line:#33301f;
    --gold-dark:#FAC74D;
    --logo:#D99E1A;
    --focus:#8a5f00;
    --radius:16px;
  }
  @media (prefers-color-scheme: dark){
    :root{
      --bg:#0d0c07;
      --surface:#16140d;
      --surface-2:#1e1b11;
      --ink:#f4f0e6;
      --ink-soft:#b7b1a2;
      --line:#2c2919;
      --gold:#FAC74D;
      --gold-deep:#FAC74D;
      --logo:#FAC74D;
      --focus:#FAC74D;
    }
  }
  *{box-sizing:border-box}
  html{scroll-behavior:smooth}
  @media (prefers-reduced-motion: reduce){html{scroll-behavior:auto}}
  body{
    margin:0;
    font-family:-apple-system,BlinkMacSystemFont,system-ui,"Segoe UI",Roboto,sans-serif;
    background:var(--bg);
    color:var(--ink);
    line-height:1.6;
    -webkit-font-smoothing:antialiased;
    text-rendering:optimizeLegibility;
  }
  a{color:inherit}
  a:focus-visible,button:focus-visible{
    outline:3px solid var(--focus);
    outline-offset:3px;
    border-radius:6px;
  }
  .wrap{max-width:1080px;margin:0 auto;padding:0 20px}
  .skip{
    position:absolute;left:-9999px;top:0;background:var(--gold);color:#111008;
    padding:10px 16px;border-radius:8px;font-weight:700;z-index:20;
  }
  .skip:focus{left:12px;top:12px}
  .brand-logo{fill:none;stroke:var(--logo)}
  .brand-logo .fillmark{fill:var(--logo);stroke:none}

  header.top{
    position:sticky;top:0;z-index:10;
    background:color-mix(in srgb, var(--bg) 88%, transparent);
    backdrop-filter:saturate(140%) blur(10px);
    border-bottom:1px solid var(--line);
  }
  @supports not (background:color-mix(in srgb,#000 50%,transparent)){
    header.top{background:var(--bg)}
  }
  .navrow{display:flex;align-items:center;justify-content:space-between;height:60px;gap:16px}
  .brand{display:flex;align-items:center;gap:10px;font-weight:800;letter-spacing:-.01em;text-decoration:none}
  .brand svg{display:block;flex:0 0 auto}
  .brand .name{font-size:1.05rem}
  .nav-links{display:none;gap:24px;align-items:center}
  .nav-links a{text-decoration:none;color:var(--ink-soft);font-weight:600;font-size:.95rem;padding:10px 2px}
  .nav-links a:hover{color:var(--ink)}
  @media (min-width:820px){.nav-links{display:flex}}

  .btn{
    display:inline-flex;align-items:center;gap:10px;
    font-weight:700;text-decoration:none;border-radius:12px;
    padding:13px 20px;font-size:1rem;line-height:1;border:1px solid transparent;
    transition:transform .15s ease, box-shadow .15s ease;
  }
  .btn-gold{background:var(--gold);color:#111008;box-shadow:0 6px 22px rgba(217,158,26,.28)}
  .btn-gold:hover{transform:translateY(-1px)}
  .btn-ghost{border-color:var(--line);color:var(--ink);background:transparent}
  .btn-ghost:hover{border-color:var(--gold)}
  .btn svg{flex:0 0 auto}
  @media (prefers-reduced-motion: reduce){.btn{transition:none}.btn-gold:hover{transform:none}}
  .btn-sm{padding:11px 16px;font-size:.9rem;min-height:44px}
  .btn .lbl{display:flex;flex-direction:column;line-height:1.05;text-align:left}
  .btn .lbl small{font-size:.66rem;font-weight:600;opacity:.78;letter-spacing:.02em}
  .btn .lbl b{font-size:1rem;font-weight:800}

  /* HERO */
  .hero{
    background:
      radial-gradient(120% 90% at 82% 8%, rgba(250,199,77,.14), transparent 55%),
      radial-gradient(90% 70% at 10% 100%, rgba(250,199,77,.07), transparent 60%),
      var(--near-black);
    color:var(--on-dark);
    position:relative;overflow:hidden;
    border-bottom:1px solid var(--dark-line);
  }
  .hero .wrap{position:relative;z-index:2;padding-top:78px;padding-bottom:76px}
  .kicker{
    display:inline-flex;align-items:center;gap:9px;flex-wrap:wrap;
    border:1px solid rgba(250,199,77,.4);
    color:var(--gold-dark);
    padding:7px 14px;border-radius:999px;font-size:.82rem;font-weight:700;
    letter-spacing:.02em;margin-bottom:26px;
  }
  .kicker .dot{width:7px;height:7px;border-radius:50%;background:var(--gold-dark);box-shadow:0 0 0 4px rgba(250,199,77,.18)}
  h1{
    font-size:clamp(2.5rem,8vw,5rem);
    line-height:1.02;letter-spacing:-.035em;font-weight:850;margin:0 0 22px;
    max-width:14ch;color:var(--on-dark);
  }
  h1 .lit{color:var(--gold-dark)}
  .lede{
    font-size:clamp(1.05rem,2.6vw,1.35rem);
    color:var(--on-dark-soft);max-width:46ch;margin:0 0 32px;line-height:1.5;
  }
  .cta-row{display:flex;flex-wrap:wrap;gap:14px;align-items:center}
  .hero .btn-ghost{border-color:rgba(250,199,77,.35);color:var(--on-dark)}
  .hero .btn-ghost:hover{border-color:var(--gold-dark)}
  .hero-note{margin-top:22px;color:var(--on-dark-soft);font-size:.9rem}
  .proof{display:flex;flex-wrap:wrap;gap:10px;margin-top:26px;list-style:none;padding:0}
  .proof li{
    display:inline-flex;align-items:center;gap:8px;
    border:1px solid var(--dark-line);border-radius:999px;
    padding:8px 14px;font-size:.85rem;font-weight:600;color:var(--on-dark-soft);
  }
  .proof li b{color:var(--on-dark);font-weight:700}
  .proof .tick{color:var(--gold-dark);display:inline-flex}

  .coins{
    position:absolute;right:-40px;top:0;bottom:0;width:52%;
    z-index:1;opacity:.9;pointer-events:none;
    display:none;
  }
  @media (min-width:900px){.coins{display:block}}
  .coin{transform-box:fill-box;transform-origin:center}
  @keyframes floaty{0%,100%{transform:translateY(0)}50%{transform:translateY(-9px)}}
  @media (prefers-reduced-motion: no-preference){
    .coin.a{animation:floaty 6s ease-in-out infinite}
    .coin.b{animation:floaty 7.5s ease-in-out infinite .6s}
    .coin.c{animation:floaty 5.4s ease-in-out infinite .3s}
    .coin.d{animation:floaty 8s ease-in-out infinite 1s}
  }

  /* generic sections */
  section{padding:76px 0}
  .eyebrow{
    text-transform:uppercase;letter-spacing:.14em;font-size:.76rem;font-weight:800;
    color:var(--gold-deep);margin:0 0 12px;
  }
  h2{font-size:clamp(1.7rem,4.4vw,2.5rem);line-height:1.12;letter-spacing:-.02em;font-weight:800;margin:0 0 16px}
  h3{font-size:1.15rem;letter-spacing:-.01em;margin:0 0 8px;font-weight:750}
  .section-lede{color:var(--ink-soft);max-width:58ch;font-size:1.08rem;margin:0 0 40px}

  /* wedge compare */
  .compare{display:grid;gap:18px;grid-template-columns:1fr}
  @media (min-width:760px){.compare{grid-template-columns:1fr 1fr}}
  .ccard{border:1px solid var(--line);border-radius:var(--radius);padding:26px;background:var(--surface)}
  .ccard.win{border-color:var(--gold);background:var(--bg);box-shadow:0 10px 40px rgba(217,158,26,.12)}
  .ctag{
    display:inline-block;font-size:.7rem;font-weight:800;letter-spacing:.08em;text-transform:uppercase;
    padding:5px 11px;border-radius:999px;margin-bottom:14px;
  }
  .ctag.dim{background:var(--surface-2);color:var(--ink-soft);border:1px solid var(--line)}
  .ctag.hot{background:var(--gold);color:#111008}
  .ccard .intro{margin:0 0 14px;color:var(--ink-soft)}
  .ccard ul{list-style:none;margin:0;padding:0;display:grid;gap:11px}
  .ccard li{display:flex;gap:11px;align-items:flex-start;font-size:.96rem;color:var(--ink)}
  .ccard li svg{flex:0 0 auto;margin-top:2px}

  /* wedge feature strip */
  .wedge-grid{display:grid;gap:18px;grid-template-columns:1fr;margin-top:44px}
  @media (min-width:820px){.wedge-grid{grid-template-columns:1fr 1fr}}
  .wcard{border:1px solid var(--line);border-radius:var(--radius);padding:26px;background:var(--surface)}
  .wcard .ic{
    width:44px;height:44px;border-radius:11px;display:flex;align-items:center;justify-content:center;
    background:rgba(217,158,26,.14);margin-bottom:16px;
  }
  .wcard p{margin:0;color:var(--ink-soft)}

  /* mandate */
  .mandate{background:var(--surface);border-top:1px solid var(--line);border-bottom:1px solid var(--line)}
  .timeline{display:grid;gap:16px;grid-template-columns:1fr}
  @media (min-width:640px){.timeline{grid-template-columns:repeat(2,1fr)}}
  @media (min-width:960px){.timeline{grid-template-columns:repeat(4,1fr)}}
  .tcard{
    background:var(--bg);border:1px solid var(--line);border-radius:var(--radius);
    padding:22px 20px;position:relative;
  }
  .tcard.live{border-color:var(--gold)}
  .tcard .yr{font-size:1.5rem;font-weight:850;letter-spacing:-.02em}
  .tcard .place{font-weight:700;margin-top:2px}
  .tcard .meta{color:var(--ink-soft);font-size:.92rem;margin-top:6px}
  .tag-live{
    display:inline-flex;align-items:center;gap:7px;margin-top:12px;font-size:.72rem;font-weight:800;letter-spacing:.04em;
    text-transform:uppercase;color:#111008;background:var(--gold);padding:4px 10px;border-radius:999px;
  }
  .tag-live .pulse{width:7px;height:7px;border-radius:50%;background:#111008}
  @media (prefers-reduced-motion: no-preference){
    .tag-live .pulse{animation:pulse 1.8s ease-in-out infinite}
  }
  @keyframes pulse{0%,100%{opacity:1}50%{opacity:.25}}
  .tag-next{
    display:inline-block;margin-top:12px;font-size:.72rem;font-weight:800;letter-spacing:.04em;
    text-transform:uppercase;color:var(--ink-soft);border:1px solid var(--line);padding:3px 9px;border-radius:999px;
  }

  /* pricing */
  .plans{display:grid;gap:20px;grid-template-columns:1fr}
  @media (min-width:900px){.plans{grid-template-columns:repeat(3,1fr);align-items:start}}
  .plan{
    border:1px solid var(--line);border-radius:20px;padding:28px 24px;background:var(--bg);
    display:flex;flex-direction:column;height:100%;
  }
  .plan.feature{border-color:var(--gold);box-shadow:0 10px 40px rgba(217,158,26,.14);position:relative}
  .plan .ptitle{display:flex;align-items:baseline;justify-content:space-between;gap:10px;margin-bottom:4px}
  .plan h3{margin:0;font-size:1.35rem}
  .price{font-size:1rem;font-weight:700;color:var(--ink-soft)}
  .plan .blurb{color:var(--ink-soft);margin:6px 0 18px;font-size:.96rem}
  .best{
    position:absolute;top:-13px;left:24px;background:var(--gold);color:#111008;
    font-size:.72rem;font-weight:850;letter-spacing:.05em;text-transform:uppercase;
    padding:5px 12px;border-radius:999px;
  }
  ul.feats{list-style:none;margin:0 0 22px;padding:0;display:grid;gap:11px}
  ul.feats li{display:flex;gap:11px;align-items:flex-start;font-size:.96rem}
  ul.feats li svg{flex:0 0 auto;margin-top:3px}
  .plan .foot{margin-top:auto}
  .plan .foot .fine{font-size:.8rem;color:var(--ink-soft);margin-top:10px}

  /* who */
  .who-grid{display:grid;gap:14px;grid-template-columns:1fr}
  @media (min-width:560px){.who-grid{grid-template-columns:1fr 1fr}}
  @media (min-width:900px){.who-grid{grid-template-columns:repeat(3,1fr)}}
  .who{border:1px solid var(--line);border-radius:14px;padding:18px 18px;background:var(--surface);font-weight:650}
  .who span{display:block;color:var(--ink-soft);font-weight:400;font-size:.9rem;margin-top:4px}

  /* promise */
  .promise{background:var(--near-black);color:var(--on-dark);border-radius:24px;padding:44px 32px;margin-top:8px}
  .promise h2{color:var(--on-dark)}
  .promise p{color:var(--on-dark-soft);max-width:62ch;font-size:1.06rem}
  .promise .pl{color:var(--gold-dark);font-weight:700}
  .privacy-row{display:flex;gap:12px;align-items:flex-start;margin-top:24px;padding-top:22px;border-top:1px solid var(--dark-line)}
  .privacy-row svg{flex:0 0 auto;margin-top:2px}
  .privacy-row p{margin:0}

  /* final cta */
  .final{text-align:center}
  .final h2{max-width:20ch;margin-left:auto;margin-right:auto}
  .final .section-lede{margin-left:auto;margin-right:auto;text-align:center}

  footer{background:var(--surface);border-top:1px solid var(--line);padding:48px 0 40px;font-size:.92rem}
  .foot-grid{display:flex;flex-wrap:wrap;gap:24px;justify-content:space-between;align-items:flex-start}
  .foot-links{display:flex;flex-wrap:wrap;gap:20px}
  .foot-links a{color:var(--ink-soft);text-decoration:none;font-weight:600;padding:8px 0;display:inline-block}
  .foot-links a:hover{color:var(--ink)}
  .foot-meta{color:var(--ink-soft);margin-top:22px;line-height:1.7}
  .foot-meta a{color:var(--gold-deep);font-weight:600}
  .disclosure{color:var(--ink-soft);font-size:.82rem;margin-top:14px;max-width:70ch}
  .disclosure a{color:var(--gold-deep)}
</style>
</head>
<body>
<a class="skip" href="#main">Skip to content</a>

<header class="top">
  <div class="wrap navrow">
    <a class="brand" href="#top" aria-label="Pay Day home">
      <svg class="brand-logo" width="30" height="30" viewBox="0 0 40 40" aria-hidden="true" focusable="false">
        <circle cx="20" cy="20" r="18" stroke-width="2.4"/>
        <circle cx="20" cy="20" r="12.5" stroke-width="1.4" opacity=".55"/>
        <path d="M20 11.5v17M16 15.5h5.4a2.6 2.6 0 0 1 0 5.2H16.6M16 20.7h5.6a2.7 2.7 0 0 1 0 5.4H16" stroke-width="2.3" stroke-linecap="round" stroke-linejoin="round"/>
      </svg>
      <span class="name">Pay&nbsp;Day</span>
    </a>
    <nav class="nav-links" aria-label="Primary">
      <a href="#mandate">The mandate</a>
      <a href="#features">Why it's different</a>
      <a href="#pricing">Pricing</a>
    </nav>
    <a class="btn btn-gold btn-sm" href="https://apps.apple.com/app/id6779927672" aria-label="Download Pay Day on the App Store">
      <svg width="16" height="19" viewBox="0 0 384 512" aria-hidden="true" focusable="false"><path fill="#111008" d="M318.7 268.7c-.2-36.7 16.4-64.4 50-84.8-18.8-26.9-47.2-41.7-84.7-44.6-35.5-2.8-74.3 20.7-88.5 20.7-15 0-49.4-19.7-76.4-19.7C63.3 141.2 4 184.8 4 273.5q0 39.3 14.4 81.2c12.8 36.7 59 126.7 107.2 125.2 25.2-.6 43-17.9 75.8-17.9 31.8 0 48.3 17.9 76.4 17.9 48.6-.7 90.4-82.5 102.6-119.3-65.2-30.7-61.7-90-61.7-91.9zm-56.6-164.2c27.3-32.4 24.8-61.9 24-72.5-24.1 1.4-52 16.4-67.9 34.9-17.5 19.8-27.8 44.3-25.6 71.9 26.1 2 49.9-11.4 69.5-34.3z"/></svg>
      <span class="lbl"><small>Download on the</small><b>App Store</b></span>
    </a>
  </div>
</header>

<main id="main">
  <span id="top"></span>
  <!-- HERO -->
  <section class="hero" aria-labelledby="hero-title">
    <svg class="coins" viewBox="0 0 520 620" aria-hidden="true" focusable="false" preserveAspectRatio="xMidYMid meet">
      <defs>
        <radialGradient id="cg" cx="38%" cy="32%" r="75%">
          <stop offset="0%" stop-color="#FFE29A"/>
          <stop offset="55%" stop-color="#FAC74D"/>
          <stop offset="100%" stop-color="#C98A12"/>
        </radialGradient>
      </defs>
      <g class="coin a" opacity=".96">
        <ellipse cx="330" cy="150" rx="86" ry="86" fill="url(#cg)"/>
        <ellipse cx="330" cy="150" rx="66" ry="66" fill="none" stroke="#7d5300" stroke-width="2" opacity=".5"/>
        <text x="330" y="168" text-anchor="middle" font-family="system-ui" font-size="62" font-weight="800" fill="#5a3c00">€</text>
      </g>
      <g class="coin b" opacity=".92">
        <ellipse cx="180" cy="300" rx="58" ry="58" fill="url(#cg)"/>
        <ellipse cx="180" cy="300" rx="44" ry="44" fill="none" stroke="#7d5300" stroke-width="1.6" opacity=".5"/>
        <text x="180" y="315" text-anchor="middle" font-family="system-ui" font-size="42" font-weight="800" fill="#5a3c00">€</text>
      </g>
      <g class="coin c" opacity=".9">
        <ellipse cx="400" cy="360" rx="46" ry="46" fill="url(#cg)"/>
        <ellipse cx="400" cy="360" rx="34" ry="34" fill="none" stroke="#7d5300" stroke-width="1.4" opacity=".5"/>
        <text x="400" y="373" text-anchor="middle" font-family="system-ui" font-size="34" font-weight="800" fill="#5a3c00">€</text>
      </g>
      <g class="coin d" opacity=".85">
        <ellipse cx="250" cy="470" rx="38" ry="38" fill="url(#cg)"/>
        <ellipse cx="250" cy="470" rx="28" ry="28" fill="none" stroke="#7d5300" stroke-width="1.3" opacity=".5"/>
        <text x="250" y="481" text-anchor="middle" font-family="system-ui" font-size="28" font-weight="800" fill="#5a3c00">€</text>
      </g>
      <circle cx="120" cy="180" r="4" fill="#FAC74D" opacity=".7"/>
      <circle cx="440" cy="230" r="5" fill="#FAC74D" opacity=".6"/>
      <circle cx="300" cy="540" r="4" fill="#FAC74D" opacity=".6"/>
    </svg>

    <div class="wrap">
      <span class="kicker"><span class="dot" aria-hidden="true"></span>EU e-invoicing &middot; Peppol &middot; Factur-X &middot; EN&nbsp;16931</span>
      <h1 id="hero-title">A PDF isn't enough <span class="lit">anymore.</span></h1>
      <p class="lede">Most invoice apps make a pretty PDF. Pay Day makes one your client's tax authority can actually read — built for the EU's shift to mandatory e-invoicing.</p>
      <div class="cta-row">
        <a class="btn btn-gold" href="https://apps.apple.com/app/id6779927672" aria-label="Download Pay Day on the App Store">
          <svg width="18" height="22" viewBox="0 0 384 512" aria-hidden="true" focusable="false"><path fill="#111008" d="M318.7 268.7c-.2-36.7 16.4-64.4 50-84.8-18.8-26.9-47.2-41.7-84.7-44.6-35.5-2.8-74.3 20.7-88.5 20.7-15 0-49.4-19.7-76.4-19.7C63.3 141.2 4 184.8 4 273.5q0 39.3 14.4 81.2c12.8 36.7 59 126.7 107.2 125.2 25.2-.6 43-17.9 75.8-17.9 31.8 0 48.3 17.9 76.4 17.9 48.6-.7 90.4-82.5 102.6-119.3-65.2-30.7-61.7-90-61.7-91.9zm-56.6-164.2c27.3-32.4 24.8-61.9 24-72.5-24.1 1.4-52 16.4-67.9 34.9-17.5 19.8-27.8 44.3-25.6 71.9 26.1 2 49.9-11.4 69.5-34.3z"/></svg>
          Download on the App Store
        </a>
        <a class="btn btn-ghost" href="#pricing">See what's free</a>
      </div>
      <p class="hero-note">Free unlimited invoices &amp; estimates. No account needed to start. Works fully offline.</p>
      <ul class="proof">
        <li><span class="tick" aria-hidden="true"><svg width="15" height="15" viewBox="0 0 24 24" fill="none"><path d="m5 12 4 4 10-10" stroke="#FAC74D" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg></span> <b>Unlimited</b>, no watermark</li>
        <li><span class="tick" aria-hidden="true"><svg width="15" height="15" viewBox="0 0 24 24" fill="none"><path d="m5 12 4 4 10-10" stroke="#FAC74D" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg></span> Real <b>EN&nbsp;16931</b> VAT engine</li>
        <li><span class="tick" aria-hidden="true"><svg width="15" height="15" viewBox="0 0 24 24" fill="none"><path d="m5 12 4 4 10-10" stroke="#FAC74D" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg></span> Data <b>stays on device</b></li>
      </ul>
    </div>
  </section>

  <!-- MANDATE -->
  <section class="mandate" id="mandate" aria-labelledby="mandate-title">
    <div class="wrap">
      <p class="eyebrow">The deadline is real</p>
      <h2 id="mandate-title">E-invoicing is becoming law across the EU.</h2>
      <p class="section-lede">One by one, EU governments are making structured e-invoices mandatory for B2B. A plain PDF won't clear their systems. Pay Day gets you ready now — not the week the rule lands.</p>
      <div class="timeline">
        <div class="tcard live">
          <div class="yr">2026</div>
          <div class="place">Belgium</div>
          <div class="meta">All businesses.</div>
          <span class="tag-live"><span class="pulse" aria-hidden="true"></span>Live now</span>
        </div>
        <div class="tcard">
          <div class="yr">2027</div>
          <div class="place">Germany</div>
          <div class="meta">Rolling out.</div>
          <span class="tag-next">Upcoming</span>
        </div>
        <div class="tcard">
          <div class="yr">2027</div>
          <div class="place">France</div>
          <div class="meta">Rolling out.</div>
          <span class="tag-next">Upcoming</span>
        </div>
        <div class="tcard">
          <div class="yr">Next</div>
          <div class="place">Poland &amp; more</div>
          <div class="meta">KSeF and others following.</div>
          <span class="tag-next">Upcoming</span>
        </div>
      </div>
    </div>
  </section>

  <!-- WEDGE / FEATURES -->
  <section id="features" aria-labelledby="feat-title">
    <div class="wrap">
      <p class="eyebrow">Why Pay Day is different</p>
      <h2 id="feat-title">One file. Clean for your client, readable by the machine.</h2>
      <p class="section-lede">The invoice a human sees and the data a tax system reads come from the same source — so the numbers can never disagree.</p>

      <div class="compare">
        <div class="ccard">
          <span class="ctag dim">Typical invoice app</span>
          <p class="intro">A nice-looking document — and nothing underneath it.</p>
          <ul>
            <li><svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="9" stroke="#9a9484" stroke-width="1.7"/><path d="M8.5 12h7" stroke="#9a9484" stroke-width="1.9" stroke-linecap="round"/></svg><span>Produces a flat PDF a human reads but a system can't parse</span></li>
            <li><svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="9" stroke="#9a9484" stroke-width="1.7"/><path d="M8.5 12h7" stroke="#9a9484" stroke-width="1.9" stroke-linecap="round"/></svg><span>Manual re-keying at the other end</span></li>
            <li><svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="9" stroke="#9a9484" stroke-width="1.7"/><path d="M8.5 12h7" stroke="#9a9484" stroke-width="1.9" stroke-linecap="round"/></svg><span>Not ready for mandatory e-invoicing rules</span></li>
          </ul>
        </div>
        <div class="ccard win">
          <span class="ctag hot">Pay Day</span>
          <p class="intro">One file, two readers — the human and the machine, always in agreement.</p>
          <ul>
            <li><svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Factur-X / ZUGFeRD: a clean PDF/A-3 carrying embedded EN&nbsp;16931 XML</span></li>
            <li><svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Delivered over Peppol, straight into the client's accounting system</span></li>
            <li><svg width="20" height="20" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Independently validated as valid PDF/A-3 with valid EN&nbsp;16931 XML</span></li>
          </ul>
        </div>
      </div>

      <div class="wedge-grid">
        <div class="wcard">
          <div class="ic" aria-hidden="true">
            <svg width="22" height="22" viewBox="0 0 24 24" fill="none"><path d="M6 2h9l5 5v15a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1V3a1 1 0 0 1 1-1z" stroke="#D99E1A" stroke-width="1.7" stroke-linejoin="round"/><path d="M14 2v5h5M8.5 13h7M8.5 16.5h7M8.5 9.5h3" stroke="#D99E1A" stroke-width="1.7" stroke-linecap="round"/></svg>
          </div>
          <h3>Factur-X / ZUGFeRD hybrid</h3>
          <p>A hybrid PDF that looks like a clean invoice to your client and carries embedded EN 16931 XML a tax authority can read automatically — independently validated as valid PDF/A-3 with valid EN 16931 XML.</p>
        </div>
        <div class="wcard">
          <div class="ic" aria-hidden="true">
            <svg width="22" height="22" viewBox="0 0 24 24" fill="none"><circle cx="5" cy="12" r="2.4" stroke="#D99E1A" stroke-width="1.7"/><circle cx="19" cy="5" r="2.4" stroke="#D99E1A" stroke-width="1.7"/><circle cx="19" cy="19" r="2.4" stroke="#D99E1A" stroke-width="1.7"/><path d="M7.1 11l9.8-5M7.1 13l9.8 5" stroke="#D99E1A" stroke-width="1.7"/></svg>
          </div>
          <h3>Peppol delivery</h3>
          <p>Send straight into your client's accounting system over the EU's official Peppol network — no portals, no re-keying. Delivery is brokered through an access-point gateway, not hand-rolled.</p>
        </div>
        <div class="wcard">
          <div class="ic" aria-hidden="true">
            <svg width="22" height="22" viewBox="0 0 24 24" fill="none"><path d="M4 8.5 12 4l8 4.5v7L12 20l-8-4.5v-7z" stroke="#D99E1A" stroke-width="1.7" stroke-linejoin="round"/><path d="m9 12 2 2 4-4" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg>
          </div>
          <h3>A real EN 16931 VAT engine</h3>
          <p>Standard rates, intra-community reverse charge, zero-rated and exempt — with a correctly rounded VAT breakdown. Free for everyone.</p>
        </div>
        <div class="wcard">
          <div class="ic" aria-hidden="true">
            <svg width="22" height="22" viewBox="0 0 24 24" fill="none"><rect x="3" y="4" width="18" height="16" rx="2" stroke="#D99E1A" stroke-width="1.7"/><path d="M3 9h18M8 14h5M8 17h8" stroke="#D99E1A" stroke-width="1.7" stroke-linecap="round"/></svg>
          </div>
          <h3>Live VIES validation</h3>
          <p>Check a client's VAT number against the EU VIES registry before you issue — advisory, never blocking. Offline on a job site? You can still produce the invoice.</p>
        </div>
      </div>
    </div>
  </section>

  <!-- PRICING -->
  <section id="pricing" style="background:var(--surface);border-top:1px solid var(--line);border-bottom:1px solid var(--line)" aria-labelledby="price-title">
    <div class="wrap">
      <p class="eyebrow">Honest pricing</p>
      <h2 id="price-title">Free is a complete tool — not a trial.</h2>
      <p class="section-lede">Pay only where it genuinely costs us or earns you compliance: structured e-invoices, network delivery, automation, AI. No watermark, no surprise limits.</p>
      <div class="plans">
        <!-- FREE -->
        <div class="plan">
          <div class="ptitle"><h3>Free</h3><span class="price">€0</span></div>
          <p class="blurb">Everything you need to invoice, beautifully.</p>
          <ul class="feats">
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Unlimited invoices &amp; estimates — no per-doc limit, no watermark</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Unlimited clients and your own logo</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Multi-currency with European Central Bank reference rates</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Full EN 16931 VAT engine, correctly rounded</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Works fully offline — no account required to start</span></li>
          </ul>
          <div class="foot">
            <a class="btn btn-ghost" href="https://apps.apple.com/app/id6779927672" style="width:100%;justify-content:center">Start free</a>
          </div>
        </div>

        <!-- PRO -->
        <div class="plan feature">
          <span class="best">Compliance</span>
          <div class="ptitle"><h3>Pro</h3><span class="price">Monthly / annual</span></div>
          <p class="blurb">Turn invoices into compliant e-invoices and deliver them.</p>
          <ul class="feats">
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Factur-X / ZUGFeRD hybrid e-invoices with embedded EN 16931 XML</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Peppol delivery into your client's accounting system</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Recurring invoices</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Branding: logo, accent colour, templates</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" fill="#D99E1A"/><path d="m8 12 2.5 2.5L16 9" stroke="#111008" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Live VAT validation against EU VIES before issuing</span></li>
          </ul>
          <div class="foot">
            <a class="btn btn-gold" href="https://apps.apple.com/app/id6779927672" style="width:100%;justify-content:center">Get Pro on the App&nbsp;Store</a>
            <p class="fine">Auto-renewing subscription, monthly or annual. The annual plan includes a 7-day free trial. Cancel anytime.</p>
          </div>
        </div>

        <!-- CREDITS -->
        <div class="plan">
          <div class="ptitle"><h3>Credit packs</h3><span class="price">One-time</span></div>
          <p class="blurb">Pay only for what you send. No padding into a subscription.</p>
          <ul class="feats">
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Cover each Peppol network send at pass-through cost</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Optional AI: photo of a quote or receipt to line items</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Optional AI: rough text to drafted invoice lines</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>Optional AI: a drafted payment-reminder email</span></li>
            <li><svg width="18" height="18" viewBox="0 0 24 24" fill="none" aria-hidden="true"><circle cx="12" cy="12" r="10" stroke="#D99E1A" stroke-width="1.6"/><path d="m8 12 2.5 2.5L16 9" stroke="#D99E1A" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg><span>AI is entirely optional — the app is complete without it</span></li>
          </ul>
          <div class="foot">
            <a class="btn btn-ghost" href="https://apps.apple.com/app/id6779927672" style="width:100%;justify-content:center">Buy as you go</a>
          </div>
        </div>
      </div>
    </div>
  </section>

  <!-- WHO -->
  <section id="who" aria-labelledby="who-title">
    <div class="wrap">
      <p class="eyebrow">Who it's for</p>
      <h2 id="who-title">Built for EU B2B — without a heavyweight accounting platform.</h2>
      <p class="section-lede">For people who invoice other businesses and need to be ready for e-invoicing mandates, not buried in software.</p>
      <div class="who-grid">
        <div class="who">Freelancers<span>Invoice, get paid, stay compliant.</span></div>
        <div class="who">Consultants<span>Recurring work, clean records.</span></div>
        <div class="who">Designers<span>Branded, on-brand documents.</span></div>
        <div class="who">Developers<span>Structured, machine-readable output.</span></div>
        <div class="who">Small businesses<span>B2B invoicing across the EU.</span></div>
        <div class="who">Anyone facing a mandate<span>Belgium, Germany, France, Poland &amp; more.</span></div>
      </div>
    </div>
  </section>

  <!-- PROMISE -->
  <section aria-labelledby="promise-title" style="padding-top:0">
    <div class="wrap">
      <div class="promise">
        <p class="eyebrow" style="color:var(--gold-dark)">Our honest promise</p>
        <h2 id="promise-title">No fake urgency. No nagging. No surprise limits.</h2>
        <p>The free app is a <span class="pl">complete tool</span>, not a trial. We ask you to pay only where it genuinely costs us or earns you compliance: structured e-invoices, network delivery, automation, and optional AI. Everything else — unlimited invoicing, real VAT math, multi-currency, offline — is simply yours.</p>
        <div class="privacy-row">
          <svg width="22" height="22" viewBox="0 0 24 24" fill="none" aria-hidden="true"><path d="M12 3 5 6v6c0 4.4 3 7.6 7 9 4-1.4 7-4.6 7-9V6l-7-3z" stroke="#FAC74D" stroke-width="1.6" stroke-linejoin="round"/><path d="m9 12 2 2 4-4" stroke="#FAC74D" stroke-width="1.9" stroke-linecap="round" stroke-linejoin="round"/></svg>
          <p><strong style="color:var(--on-dark)">Privacy by design.</strong> Your business and client data stays on the device. Nothing is uploaded except the documents you choose to send.</p>
        </div>
      </div>
    </div>
  </section>

  <!-- FINAL CTA -->
  <section class="final" aria-labelledby="final-title" style="padding-top:20px">
    <div class="wrap">
      <p class="eyebrow" style="text-align:center">Available now on the App Store</p>
      <h2 id="final-title">Be ready before the mandate lands.</h2>
      <p class="section-lede">Free to start, no account needed. Pay Day: E-Invoice &amp; Peppol.</p>
      <a class="btn btn-gold" href="https://apps.apple.com/app/id6779927672" aria-label="Download Pay Day on the App Store">
        <svg width="18" height="22" viewBox="0 0 384 512" aria-hidden="true" focusable="false"><path fill="#111008" d="M318.7 268.7c-.2-36.7 16.4-64.4 50-84.8-18.8-26.9-47.2-41.7-84.7-44.6-35.5-2.8-74.3 20.7-88.5 20.7-15 0-49.4-19.7-76.4-19.7C63.3 141.2 4 184.8 4 273.5q0 39.3 14.4 81.2c12.8 36.7 59 126.7 107.2 125.2 25.2-.6 43-17.9 75.8-17.9 31.8 0 48.3 17.9 76.4 17.9 48.6-.7 90.4-82.5 102.6-119.3-65.2-30.7-61.7-90-61.7-91.9zm-56.6-164.2c27.3-32.4 24.8-61.9 24-72.5-24.1 1.4-52 16.4-67.9 34.9-17.5 19.8-27.8 44.3-25.6 71.9 26.1 2 49.9-11.4 69.5-34.3z"/></svg>
        Download on the App Store
      </a>
    </div>
  </section>
</main>

<footer aria-labelledby="foot-title">
  <div class="wrap">
    <h2 id="foot-title" style="position:absolute;width:1px;height:1px;overflow:hidden;clip:rect(0 0 0 0);clip-path:inset(50%);white-space:nowrap">Footer</h2>
    <div class="foot-grid">
      <a class="brand" href="#top" aria-label="Pay Day home" style="color:var(--ink)">
        <svg class="brand-logo" width="26" height="26" viewBox="0 0 40 40" aria-hidden="true" focusable="false">
          <circle cx="20" cy="20" r="18" stroke-width="2.4"/>
          <path d="M20 11.5v17M16 15.5h5.4a2.6 2.6 0 0 1 0 5.2H16.6M16 20.7h5.6a2.7 2.7 0 0 1 0 5.4H16" stroke-width="2.3" stroke-linecap="round" stroke-linejoin="round"/>
        </svg>
        <span class="name">Pay&nbsp;Day</span>
      </a>
      <nav class="foot-links" aria-label="Footer">
        <a href="/privacy/payday">Privacy</a>
        <a href="/terms/payday">Terms</a>
        <a href="/support/payday">Support</a>
        <a href="https://apps.apple.com/app/id6779927672">App Store</a>
      </nav>
    </div>
    <p class="foot-meta">
      Pay Day: E-Invoice &amp; Peppol &mdash; developed by <strong>Midgar Oy</strong>, Helsinki, Finland.<br>
      <a href="https://apps.apple.com/app/id6779927672">View on the App Store &rarr;</a>
    </p>
    <p class="disclosure">
      Pro is an auto-renewing subscription (monthly or annual); the annual plan includes a 7-day free trial. payment is charged to your Apple Account and renews unless cancelled at least 24 hours before the period ends. Credit packs are one-time purchases. See <a href="/terms/payday">Terms</a> for full details.
    </p>
  </div>
</footer>
</body>
</html>"###;
    Response::ok(html).map(|mut r| {
        let _ = r.headers_mut().set("Content-Type", "text/html; charset=utf-8");
        r
    })
}

pub fn terms_handler(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let app = ctx.param("app").map(|s| s.as_str()).unwrap_or("");
    Response::ok(render_terms(app)).map(|mut r| {
        let _ = r.headers_mut().set("Content-Type", "text/html; charset=utf-8");
        r
    })
}

pub fn support_handler(_req: Request, ctx: RouteContext<()>) -> Result<Response> {
    let app = ctx.param("app").map(|s| s.as_str()).unwrap_or("");
    Response::ok(render_support(app)).map(|mut r| {
        let _ = r.headers_mut().set("Content-Type", "text/html; charset=utf-8");
        r
    })
}

fn page(name: &str, title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>{name} — {title}</title>
<style>body{{font-family:-apple-system,BlinkMacSystemFont,system-ui,sans-serif;max-width:680px;margin:40px auto;padding:0 20px;line-height:1.6;color:#1c1c1e}}h1{{font-size:28px}}h2{{font-size:18px;margin-top:28px}}a{{color:#06c}}@media(prefers-color-scheme:dark){{body{{background:#000;color:#e5e5ea}}a{{color:#4da3ff}}}}</style>
</head><body>{body}</body></html>"#,
        name = name,
        title = title,
        body = body,
    )
}

fn render_terms(app: &str) -> String {
    let c = config(app);
    let subscriptions = if app == "livingdex" {
        "Living Dex Pro is an auto-renewing subscription sold through Apple In-App Purchase. Payment is charged to your Apple Account at confirmation. It renews automatically unless cancelled at least 24 hours before the end of the current period; manage or cancel it in your Apple Account settings. Any free-trial portion is forfeited when you purchase a subscription. Basic identification is free. Prices are shown in the App before purchase."
    } else {
        "Pay Day Pro is an auto-renewing subscription sold through Apple In-App Purchase. Payment is charged to your Apple Account at confirmation. It renews automatically unless cancelled at least 24 hours before the end of the current period; manage or cancel it in your Apple Account settings. Any free-trial portion is forfeited when you purchase a subscription. Credit packs are one-time consumable purchases used to send invoices over the Peppol network and for optional AI features; consumed credits are non-refundable. Prices are shown in the App before purchase."
    };
    let responsibilities = if app == "livingdex" {
        "Identifications are AI-generated best guesses and may be wrong. <strong>Never eat, touch, or handle any wild plant, fungus, or animal based on the App's identification</strong> — many species are toxic, venomous, protected, or dangerous, and misidentification can cause serious harm. The App is for general education and enjoyment and is not safety, medical, foraging, or professional advice. Respect wildlife and habitats and follow all local laws and protected-area rules."
    } else {
        "You are responsible for the accuracy and legality of the invoices, client data, and tax information you enter, and for having a lawful basis to invoice your clients. The App helps you produce documents in standard formats (including EN 16931 / Peppol); it is not tax, accounting, or legal advice, and you remain responsible for your compliance obligations."
    };
    let acceptable = if app == "livingdex" {
        "Do not use the App unlawfully, to harass, bait, or endanger wildlife, to disturb protected species, nests, or habitats, to infringe others' rights, or to interfere with or reverse-engineer the service."
    } else {
        "Do not use the App unlawfully, to send fraudulent or unsolicited documents, to infringe others' rights, or to interfere with or reverse-engineer the service."
    };
    let availability = if app == "livingdex" {
        "Cloud identification and AI features depend on third-party services and a network connection and may be unavailable or delayed; on-device features work offline."
    } else {
        "Network features (VAT validation, currency rates, and Peppol delivery) depend on third-party services and may be unavailable or delayed; issuing and sharing invoices as PDFs does not require them."
    };
    let body = format!(
        r#"<h1>{name} Terms of Use</h1>
<p><em>Last updated: July 2026</em></p>
<p>These Terms govern your use of {name} (the "App"), operated by Midgar Oy. By downloading or using the App you agree to them.</p>
<h2>Licence</h2>
<p>We grant you a personal, non-transferable, revocable licence to use the App on Apple devices you own or control, in accordance with the Apple Media Services Terms and these Terms.</p>
<h2>Subscriptions and purchases</h2>
<p>{subscriptions}</p>
<h2>Your responsibilities</h2>
<p>{responsibilities}</p>
<h2>Acceptable use</h2>
<p>{acceptable}</p>
<h2>Availability and third parties</h2>
<p>{availability}</p>
<h2>Disclaimer and liability</h2>
<p>The App is provided "as is" without warranties of any kind. To the maximum extent permitted by law, Midgar Oy is not liable for indirect or consequential damages, and our total liability is limited to the amount you paid for the App in the 12 months before the claim. Nothing limits liability that cannot be excluded by law.</p>
<h2>Changes and termination</h2>
<p>We may update these Terms; continued use after an update constitutes acceptance. We may suspend the service for misuse.</p>
<h2>Governing law</h2>
<p>These Terms are governed by the laws of Finland, without regard to conflict-of-laws rules.</p>
<h2>Contact</h2>
<p>{contact}</p>"#,
        name = c.name,
        contact = contact_link(app),
    );
    page(c.name, "Terms of Use", &body)
}

fn render_support(app: &str) -> String {
    let c = config(app);
    let topics = if app == "livingdex" {
        r#"<li>Subscriptions are managed in your Apple Account settings; restore purchases from the paywall.</li>
<li>Your collection and photos are stored on your device; deleting the app removes them.</li>
<li>Identifications are AI best guesses — never eat, touch, or handle a wild organism based only on the App.</li>"#
    } else {
        r#"<li>Subscriptions and credit packs are managed in your Apple Account settings; restore purchases from the paywall.</li>
<li>You can delete your account from within the app (Settings).</li>
<li>Invoices and client data are stored on your device; the App works offline for creating and sharing PDFs.</li>"#
    };
    let body = format!(
        r#"<h1>{name} Support</h1>
<p>Need help with {name}? We're happy to assist.</p>
<h2>Contact</h2>
<p>Email us at {contact} and we'll get back to you, usually within two business days.</p>
<h2>Common topics</h2>
<ul>
{topics}
</ul>
<h2>Legal</h2>
<p>See our <a href="/privacy/{app}">Privacy Policy</a> and <a href="/terms/{app}">Terms of Use</a>.</p>"#,
        name = c.name,
        contact = contact_link(app),
        app = app,
    );
    page(c.name, "Support", &body)
}
