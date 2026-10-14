//! Standard `<video>`/`<audio>` playback attributes: `poster`, `autoplay`,
//! `muted`, `loop`, `preload`, `playsinline`, `crossorigin`, plus the
//! `<source>` child list the resource is chosen from.
//!
//! Parsing lives here (one place, unit-tested against the HTML rules) and the
//! engine consumes the result in the web-media pipeline: the poster rides the
//! frame image's payload, `preload="none"` defers the source fetch until
//! playback, `autoplay` starts the transport, `loop` rewinds it at the end,
//! and [`media_candidates`] is the ordered candidate list the pipeline walks
//! (first one that decodes wins, the element's own `src` last). Everything
//! here is engine-internal — none of it changes the frame wire (still v20),
//! because each behavior rides state the v20 media section already carries
//! (`playing`/`position`), the media item's `src`, or a raster payload id.
//!
//! Deliberately not honored (no engine path for them): `loading=lazy` and
//! `controlslist`.
//!
//! `playsinline` is parsed and then deliberately unused: playback is always
//! inline (there is no fullscreen route), so the flag must not gate anything
//! — not autoplay, not the deferred load. It is read so the no-op is a
//! recorded decision rather than an oversight (see the field docs and
//! `playsinline_never_gates_playback`).

use kitae_dom::{Dom, ElementData, NodeId};
use kitae_style::EnvironmentState;

/// The `preload` content attribute (an HTML enumerated attribute).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Preload {
    /// `auto`, `preload=""`, or a missing/invalid value: load the source.
    #[default]
    Auto,
    /// `metadata`: fetch the resource's metadata. The engine's fetcher has no
    /// byte-range support, so a metadata preload is the same single GET the
    /// automatic state makes — this value currently only records the author's
    /// intent (and differs from `none`).
    Metadata,
    /// `none`: do not fetch until playback is requested.
    None,
}

/// The `crossorigin` content attribute (an HTML enumerated attribute): which
/// CORS mode the element's media fetch runs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum CrossOrigin {
    /// A missing, explicitly `no-cors`, or invalid value: an opaque no-CORS
    /// fetch — what every other subresource in the engine makes, and the
    /// missing-value default HTML specifies.
    #[default]
    NoCors,
    /// `crossorigin` (valueless, i.e. the empty string) or
    /// `crossorigin="anonymous"`: a CORS fetch *without* credentials. The
    /// engine holds no cookie jar and no origin store, so there is nothing
    /// credential-shaped to leak and the request is served like no-cors.
    Anonymous,
    /// `crossorigin="use-credentials"`: a CORS fetch *with* credentials. The
    /// engine has no cookie jar and no way to satisfy a credentialed CORS
    /// check, so this fails closed (see [`MediaAttrs::fetch_allowed`]).
    UseCredentials,
}

/// The media-element attributes the engine acts on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MediaAttrs {
    pub autoplay: bool,
    pub muted: bool,
    pub looping: bool,
    pub preload: Preload,
    /// The `poster` attribute, trimmed; `None` when absent or empty (HTML:
    /// an empty/absent poster attribute means "no poster frame").
    pub poster: Option<String>,
    /// The `crossorigin` mode the element's media fetch runs in.
    pub crossorigin: CrossOrigin,
    /// The `playsinline` flag, recorded and never acted on: the engine only
    /// ever plays media inline, so honoring the flag and ignoring it are the
    /// same behavior — and *disabling* playback for it would not be.
    pub playsinline: bool,
}

impl MediaAttrs {
    /// Whether playback may start on its own, without a user gesture.
    ///
    /// The engine demuxes only the *video* track of a `<video>` (there is no
    /// audio handoff for web video), so a video is always silent and never
    /// blocks on a mute flag. `<audio>` does hand decoded PCM to the host's
    /// player, so it autoplays only when `muted` — the audible-autoplay case
    /// that the spec's "must not play audible media without a user gesture"
    /// rule is about.
    pub(crate) fn autoplay_allowed(&self, is_video: bool) -> bool {
        self.autoplay && (is_video || self.muted)
    }

    /// Whether the source fetch waits for playback to be requested.
    pub(crate) fn defers_load(&self) -> bool {
        self.preload == Preload::None
    }

    /// Whether the engine may fetch the element's media resource at all.
    ///
    /// `crossorigin="use-credentials"` asks for a credentialed CORS fetch.
    /// The engine has no credential store and no origin to run a CORS check
    /// against, so honoring it is not possible and *approximating* it would
    /// send the request anyway — the one outcome the attribute exists to
    /// prevent. It fails closed instead: nothing is fetched or decoded, the
    /// page's transport stays empty, and the media item publishes no URL for
    /// the host to chase.
    pub(crate) fn fetch_allowed(&self) -> bool {
        self.crossorigin != CrossOrigin::UseCredentials
    }
}

/// Read the playback attributes off a media element. `autoplay`, `muted` and
/// `loop` are boolean attributes (presence is true — `muted="false"` mutes,
/// the same way `checked="false"` checks).
pub(crate) fn media_attrs(el: Option<&ElementData>) -> MediaAttrs {
    let Some(el) = el else {
        return MediaAttrs::default();
    };
    MediaAttrs {
        autoplay: el.attr("autoplay").is_some(),
        muted: el.attr("muted").is_some(),
        looping: el.attr("loop").is_some(),
        preload: match el.attr("preload") {
            // Enumerated keywords are ASCII case-insensitive; the empty value
            // is Automatic. A missing or invalid value is implementation
            // defined and the engine loads.
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "none" => Preload::None,
                "metadata" => Preload::Metadata,
                _ => Preload::Auto,
            },
            None => Preload::Auto,
        },
        poster: el
            .attr("poster")
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .map(str::to_string),
        crossorigin: match el.attr("crossorigin") {
            // The empty value *is* `anonymous` for this attribute (it is not
            // one of the enumerated attributes whose empty value means the
            // default), and keywords are ASCII case-insensitive. Anything else
            // — a missing value, or a keyword the spec does not define —
            // falls back to the missing-value default, `no-cors`.
            Some(v) => match v.trim().to_ascii_lowercase().as_str() {
                "" | "anonymous" => CrossOrigin::Anonymous,
                "use-credentials" => CrossOrigin::UseCredentials,
                _ => CrossOrigin::NoCors,
            },
            None => CrossOrigin::NoCors,
        },
        playsinline: el.attr("playsinline").is_some(),
    }
}

/// The media element's candidate resources, in HTML's priority order: every
/// `<source>` child with a non-empty `src` whose `media` condition holds, in
/// document order, then the element's own `src` (skipped when absent or
/// blank). The engine walks this list and takes the first that *decodes*, so
/// the element's `src` is the fallback rather than the first choice — the one
/// place where HTML and the engine differ: the spec's algorithm would
/// pre-filter candidates by `type` against `canPlayType`, and the engine has
/// no type table, so it lets the decoder decide.
///
/// `media` runs through the same evaluator the stylesheet path uses
/// ([`crate::media_allows`]), against the live viewport and environment, so a
/// `<source media="(min-width: …)">` gates exactly like the `@media` rule
/// that says the same thing.
///
/// Values are unresolved: the caller resolves each against the page's
/// resource base (and applies the mixed-content gate) like any other
/// subresource.
pub(crate) fn media_candidates(
    dom: &Dom,
    el: Option<NodeId>,
    viewport_w: f32,
    viewport_h: f32,
    env: &EnvironmentState,
) -> Vec<String> {
    let Some(el) = el else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for child in dom.children(el) {
        let Some(source) = dom.element(*child) else {
            continue;
        };
        if source.tag != "source" {
            continue;
        }
        if !crate::media_allows(source.attr("media"), viewport_w, viewport_h, env) {
            continue;
        }
        if let Some(src) = source.attr("src").map(str::trim).filter(|s| !s.is_empty()) {
            out.push(src.to_string());
        }
    }
    if let Some(src) = dom
        .element(el)
        .and_then(|e| e.attr("src"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(src.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use kitae_html::parse;

    fn attrs_of(html: &str) -> MediaAttrs {
        let (dom, _) = parse(html);
        let root = dom.root().expect("root");
        let mut found = None;
        dom.walk(root, &mut |n| {
            if found.is_none()
                && dom
                    .element(n)
                    .is_some_and(|e| e.tag == "video" || e.tag == "audio")
            {
                found = Some(n);
            }
        });
        media_attrs(dom.element(found.expect("media element")))
    }

    #[test]
    fn defaults_are_a_plain_paused_element() {
        let a = attrs_of("<video src='c.webm'></video>");
        assert_eq!(
            a,
            MediaAttrs {
                autoplay: false,
                muted: false,
                looping: false,
                preload: Preload::Auto,
                poster: None,
                crossorigin: CrossOrigin::NoCors,
                playsinline: false,
            }
        );
        // A missing element (no node, e.g. a synthetic media item) is the
        // same as no attributes at all.
        assert_eq!(media_attrs(None), MediaAttrs::default());
    }

    #[test]
    fn boolean_attributes_key_off_presence() {
        let a = attrs_of("<video src='c.webm' autoplay muted loop></video>");
        assert!(a.autoplay && a.muted && a.looping);
        // Boolean attributes ignore their value, HTML-style: `muted="false"`
        // still reads as true.
        let a = attrs_of("<video src='c.webm' autoplay='false' muted='false' loop=''></video>");
        assert!(a.autoplay && a.muted && a.looping);
        // And a mere word is not enough.
        let a = attrs_of("<video src='c.webm' data-muted='1' mute></video>");
        assert!(!a.muted && !a.autoplay && !a.looping);
    }

    #[test]
    fn preload_keywords_and_invalid_values() {
        for (attr, want) in [
            ("preload='none'", Preload::None),
            ("preload=' NONE '", Preload::None),
            ("preload='metadata'", Preload::Metadata),
            ("preload='MetaData'", Preload::Metadata),
            ("preload='auto'", Preload::Auto),
            ("preload=''", Preload::Auto),
            ("preload='bogus'", Preload::Auto),
            ("", Preload::Auto),
        ] {
            let a = attrs_of(&format!("<video src='c.webm' {attr}></video>"));
            assert_eq!(a.preload, want, "{attr}");
        }
        assert!(attrs_of("<video preload='none'></video>").defers_load());
        assert!(!attrs_of("<video preload='metadata'></video>").defers_load());
        assert!(!attrs_of("<video></video>").defers_load());
    }

    #[test]
    fn poster_is_trimmed_and_may_be_absent() {
        assert_eq!(
            attrs_of("<video poster='  p.jpg  '></video>")
                .poster
                .as_deref(),
            Some("p.jpg")
        );
        assert_eq!(attrs_of("<video poster=''></video>").poster, None);
        assert_eq!(attrs_of("<video poster='   '></video>").poster, None);
        assert_eq!(attrs_of("<video></video>").poster, None);
    }

    #[test]
    fn autoplay_needs_mute_for_audible_media_only() {
        // Silent video autoplays either way.
        let a = attrs_of("<video src='c.webm' autoplay></video>");
        assert!(a.autoplay_allowed(true));
        assert!(!a.autoplay_allowed(false), "audible without a gesture");
        // Muted audio autoplays; unmuted does not.
        let a = attrs_of("<audio src='a.mp3' autoplay muted></audio>");
        assert!(a.autoplay_allowed(false));
        let a = attrs_of("<audio src='a.mp3' autoplay></audio>");
        assert!(!a.autoplay_allowed(false));
        // Mute alone never starts playback.
        let a = attrs_of("<video src='c.webm' muted></video>");
        assert!(!a.autoplay_allowed(true) && !a.autoplay_allowed(false));
    }

    #[test]
    fn crossorigin_keywords_decide_whether_a_fetch_may_happen() {
        for (attr, want, allowed) in [
            // A valueless `crossorigin` is the empty string, which for this
            // attribute means anonymous (not the missing-value default).
            ("crossorigin", CrossOrigin::Anonymous, true),
            ("crossorigin=''", CrossOrigin::Anonymous, true),
            ("crossorigin='anonymous'", CrossOrigin::Anonymous, true),
            ("crossorigin=' ANONYMOUS '", CrossOrigin::Anonymous, true),
            // Absent, and any keyword the spec does not define, are no-cors.
            ("", CrossOrigin::NoCors, true),
            ("crossorigin='no-cors'", CrossOrigin::NoCors, true),
            ("crossorigin='use_credentials'", CrossOrigin::NoCors, true),
            ("crossorigin='bogus'", CrossOrigin::NoCors, true),
            // The credentialed mode cannot be satisfied: no fetch at all.
            (
                "crossorigin='use-credentials'",
                CrossOrigin::UseCredentials,
                false,
            ),
            (
                "crossorigin=' Use-Credentials '",
                CrossOrigin::UseCredentials,
                false,
            ),
        ] {
            let a = attrs_of(&format!("<video src='c.webm' {attr}></video>"));
            assert_eq!(a.crossorigin, want, "{attr}");
            assert_eq!(a.fetch_allowed(), allowed, "{attr}");
        }
    }

    #[test]
    fn playsinline_never_gates_playback() {
        // The engine only ever plays inline, so the flag is recorded and
        // otherwise inert — in particular it must not stand in for "not
        // allowed to start without a gesture".
        for (html, autoplay) in [
            ("<video src='c.webm' autoplay playsinline></video>", true),
            (
                "<audio src='a.mp3' autoplay muted playsinline></audio>",
                true,
            ),
            (
                "<video src='c.webm' autoplay playsinline='false'></video>",
                true,
            ),
        ] {
            let a = attrs_of(html);
            assert!(a.playsinline, "{html}");
            assert!(a.fetch_allowed(), "{html}");
            assert!(!a.defers_load(), "{html}");
            assert!(
                a.autoplay_allowed(!html.contains("<audio")),
                "playsinline must not suppress autoplay: {html}"
            );
            assert_eq!(a.autoplay, autoplay, "{html}");
        }
        // It is not an autoplay, mute, loop or preload alias either.
        let a = attrs_of("<video src='c.webm' playsinline></video>");
        assert!(!a.autoplay && !a.muted && !a.looping, "{a:?}");
        // `preload="none"` still defers with the flag present — playsinline
        // adds no fetch of its own, it just never subtracts one.
        assert!(attrs_of("<video src='c.webm' playsinline preload='none'></video>").defers_load());
    }

    /// The media element of `html` (the first `<video>`/`<audio>`), with the
    /// DOM it sits in.
    fn media_el(html: &str) -> (Dom, Option<NodeId>) {
        let (dom, _) = parse(html);
        let root = dom.root().expect("root");
        let mut found = None;
        dom.walk(root, &mut |n| {
            if found.is_none()
                && dom
                    .element(n)
                    .is_some_and(|e| e.tag == "video" || e.tag == "audio")
            {
                found = Some(n);
            }
        });
        (dom, found)
    }

    fn candidates_of(html: &str, w: f32) -> Vec<String> {
        let (dom, el) = media_el(html);
        media_candidates(&dom, el, w, 600.0, &EnvironmentState::default())
    }

    #[test]
    fn source_children_precede_the_elements_own_src() {
        assert_eq!(
            candidates_of(
                "<video src='own.webm'><source src='a.webm'><source src='b.webm'></video>",
                400.0
            ),
            ["a.webm", "b.webm", "own.webm"],
            "document order, element src last"
        );
        // Without `<source>` children the list is the element's own `src` —
        // the single-src path the engine had before.
        assert_eq!(
            candidates_of("<video src='own.webm'></video>", 400.0),
            ["own.webm"]
        );
        // A source-only element: no element `src` to fall back to.
        assert_eq!(
            candidates_of("<video controls><source src='a.webm'></video>", 400.0),
            ["a.webm"]
        );
        // No source at all (an empty `<video>`, a missing element) is empty,
        // not a fetch of the page URL.
        assert!(candidates_of("<video></video>", 400.0).is_empty());
        let (dom, none) = (parse("<p>x</p>").0, None);
        assert!(
            media_candidates(&dom, none, 400.0, 600.0, &EnvironmentState::default()).is_empty()
        );
    }

    #[test]
    fn source_list_ignores_blank_srcs_and_non_source_children() {
        assert_eq!(
            candidates_of(
                "<video src=' own.webm '><track src='c.vtt'><source src='  '>\
                 <source src='a.webm'><source><source src='b.webm'></video>",
                400.0
            ),
            ["a.webm", "b.webm", "own.webm"],
            "blank srcs are skipped, other children are not candidates"
        );
    }

    #[test]
    fn source_media_condition_gates_the_candidate() {
        let html = "<video><source src='narrow.webm' media='(max-width: 600px)'>\
                    <source src='wide.webm' media='(min-width: 900px)'></video>";
        assert_eq!(candidates_of(html, 400.0), ["narrow.webm"]);
        assert_eq!(candidates_of(html, 1200.0), ["wide.webm"]);
        // A condition no candidate satisfies leaves nothing to fetch; a
        // missing or `all` condition always holds.
        let none = "<video><source src='a.webm' media='(min-width: 900px)'></video>";
        assert!(candidates_of(none, 400.0).is_empty());
        assert_eq!(
            candidates_of("<video><source src='a.webm' media=''></video>", 400.0),
            ["a.webm"]
        );
        assert_eq!(
            candidates_of("<video><source src='a.webm' media='all'></video>", 400.0),
            ["a.webm"]
        );
        // A comma list is OR'd, like every other media condition.
        assert_eq!(
            candidates_of(
                "<video><source src='a.webm' media='print, (max-width: 600px)'></video>",
                400.0
            ),
            ["a.webm"]
        );
    }
}
