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
    let body = r#"<h1>Pay Day</h1>
<p style="font-size:18px;color:#555">Invoices &amp; estimates for EU freelancers — beautiful, free, and ready for e-invoicing compliance.</p>
<h2>What it does</h2>
<ul>
<li>Unlimited invoices and estimates, your logo, any currency, full EU VAT math.</li>
<li><strong>Pro:</strong> EN 16931-valid Factur-X / ZUGFeRD hybrid PDFs that pass tax-authority validation.</li>
<li><strong>Pro + credits:</strong> deliver invoices over the Peppol network straight into your client's accounting system, and validate VAT numbers against EU VIES.</li>
<li>Optional AI: draft line items from a photo or a sentence, and write payment reminders.</li>
</ul>
<p>Your business and client data stays on your device; nothing is uploaded except the documents you choose to send.</p>
<h2>Links</h2>
<p><a href="/privacy/payday">Privacy Policy</a> · <a href="/terms/payday">Terms of Use</a> · <a href="/support/payday">Support</a></p>"#;
    Response::ok(page("Pay Day", "EU e-invoicing for freelancers", body)).map(|mut r| {
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
