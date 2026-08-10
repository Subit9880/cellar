//! The compiled-in filters.
//!
//! `default` is a port of ProtoCocktail's `wa-diff-analyzer` ruleset — the part of
//! that tool that was accumulated empirically against real bundles and is worth far
//! more than the code around it. It is reproduced faithfully, including its
//! judgement calls, with two structural changes:
//!
//! - The patterns that were unconditional in the original (`isUIModule` returned
//!   `true` for them *before* consulting the protocol list) are in
//!   [`Filter::hard_exclude`](crate::filter::Filter::hard_exclude) rather than mixed
//!   into the general exclusion list, so the precedence is stated instead of implied
//!   by statement order.
//! - Dependency propagation is a fixpoint over the graph rather than an
//!   order-dependent accumulation — see [`crate::filter`].
//!
//! Built-ins are read-only. `cellar filter fork default mine` copies one into
//! `filters/` where it can be edited.

use crate::filter::{Filter, Verdict};

/// Module classes handled by a *different* tool, or with no textual content worth
/// diffing. Excluded ahead of everything else, including the protocol list.
const HARD_EXCLUDE: &[&str] = &[
    r"\.react$",
    r"\.story$",
    r"\.graphql$",
    r"\.pb$",
    r"\.proto$",
    r"\.flow$",
];

/// Protocol-surface patterns. A match here overrides any exclusion and exempts the
/// module from dependency propagation.
const PROTOCOL: &[&str] = &[
    // Core crypto & Signal protocol.
    r"^WASignal",
    r"^WANoise",
    r"^WACrypto",
    r"^WAProto",
    r"^WAProtocol",
    // USync.
    r"Usync",
    // WAM tooling (the reporters and bridges, not the events or enums themselves).
    r"^WAWebWam[A-DF-Z]",
    // WID / LID / JID. Case-sensitive, and negated where a common English word
    // would otherwise match: `Wid[^t]` avoids `Width`, `[^S]Lid` avoids `Slider`.
    r"Wid[^t]",
    r"Wid$",
    r"[^S]Lid",
    r"Lid$",
    r"Jid",
    r"^WAWapJid",
    // Bridge APIs.
    r"BridgeApi$",
    // User preferences.
    r"^WAWebUserPrefs",
    // Common protocol suffixes.
    r"^WAWeb.*Action$",
    r"^WAWeb.*Job$",
    r"^WAWeb.*Api$",
    r"^WA[A-Z].*Action$",
    r"^WA[A-Z].*Job$",
    r"^WA[A-Z].*Api$",
    // Send / set / sync / query.
    r"^WAWebSend",
    r"^WAWebSet",
    r"^WAWebSyncd",
    r"^WAWebSync",
    r"^WAWebQuery",
    // Smax RPC (the typed protocol layer).
    r"^WASmax",
    // Binary / WAP / node encoding.
    r"^WABinary$",
    r"^WAWap",
    r"^WAParsable",
    r"^WAXml",
    r"^WADeprecated",
    // Socket & comms.
    r"^WAComms",
    r"^WAFrameSocket$",
    r"^WASocketTransport$",
    // Syncd & app state.
    r"^WASyncd",
    r"^WASync",
    // Message handling & processing.
    r"^WAWebHandle",
    r"^WAWebParse.*Proto$",
    r"^WAWebParse.*Message",
    r"^WAWebProcess",
    r"^WAWebMsgProcessing",
    r"^WAWebE2EProto",
    // Addon system (message extensions: reactions, polls, pins, comments).
    r"^WAWebAddon",
    // Stanza / node handling.
    r"^WAWebCommsHandle.*Stanza",
    r"^WAWebStanza",
    r"^WAWeb.*StanzaAdapters$",
    r"^WAWebSendMsg.*Stanza",
    r"^WAWebSerialize.*Node",
    // Receipts & acks.
    r"^WAWebAck",
    r"^WAWebReceipt",
    r"^WAWeb.*Receipt(?:Batcher|Utils|Parser)?$",
    // Encryption / decryption.
    r"^WAWebEncrypt",
    r"^WAWebDecrypt",
    r"^WAWebCryptoCreate",
    r"^WAWebCryptoDecrypt",
    r"^WAWebCryptoEncrypt",
    r"^WAWebCryptoMedia",
    r"^WAWebBroadcastSenderKey",
    // Database operations that carry protocol payloads.
    r"^WAWebDB(?:Encrypt|Decrypt|Create|Update|Delete|Get|Persist|Bulk)",
    r"^WAWebDBGroups",
    r"^WAWebDBMessage",
    r"^WAWebDBDevice",
    // Multi-device / companion.
    r"^WAWebAdv",
    r"^WAWebAltDeviceLinking",
    r"^WAWebCompanionReg",
    r"^WAWebAccountLinking",
    r"^WAWebDevice(?:Capabilities|Features|List|Sent)",
    // Identity & keys.
    r"^WAWebIdentity",
    r"^WAWebGetIdentity",
    r"^WAWebManageE2E",
    // History sync.
    r"^WAWebHistorySync",
    r"^WAWebHistoryMsg",
    // Offline resume.
    r"^WAWebOffline(?:Handler|Resume|Device|Simulator)",
    // VoIP / calls.
    r"^WAWebVoip",
    r"^WAWebHandleVoip",
    r"^WAWebCallLog",
    // Groups & communities.
    r"^WAWebCommunity(?:Activity|Daily|Gating|Group)",
    // Newsletters (channels).
    r"^WAWebNewsletter(?:Msg|Boot|Send|CRUD)",
    r"^WAWebBootstrapNewsletter",
    // MEX / GraphQL query drivers.
    r"^WAWebMex[A-Z]",
    // Message send flow.
    r"^WAWebSendMsg",
    r"^WAWebGroupMsgSend",
    r"^WAWebPrepareMessageSending",
    r"^WAWebMessageSend(?:Perf)?Reporter",
    // Schema definitions.
    r"^WAWebSchema",
    // Specific protocol modules.
    r"^MAW(?:Get|Mark|Message)",
    r"^MWPPending",
    // Retry / resend.
    r"^WAWebRetry",
    r"^WAWebResend",
    // Key transparency.
    r"^KeyTransparency",
    // IndexedDB core.
    r"^WAIDB",
    // Result/error plumbing.
    r"^WAResultOrError$",
];

/// UI, analytics, and the other Meta products that share the bundle.
const UI_AND_UNRELATED: &[&str] = &[
    // Analytics event/enum definitions. The WAM *tooling* is in PROTOCOL above and
    // wins; these are the generated event shapes.
    r"WamEvent$",
    r"^WAWebWamEnum",
    r"FalcoEvent$",
    r"_facebookRelayOperation$",
    // React.
    r"^use[A-Z]",
    r"(?i)Loadable$",
    // UI component suffixes.
    r"(?i)Icon$",
    r"(?i)Button$",
    r"(?i)Modal$",
    r"(?i)Drawer$",
    r"(?i)Dialog$",
    r"(?i)Popup$",
    r"(?i)Banner$",
    r"(?i)Toast$",
    r"(?i)Menu$",
    r"(?i)Dropdown$",
    r"(?i)Tooltip$",
    r"(?i)Avatar$",
    r"(?i)Spinner$",
    r"(?i)Skeleton$",
    r"(?i)Gallery$",
    r"(?i)Viewer$",
    r"(?i)Player$",
    r"(?i)Animation$",
    r"(?i)Theme$",
    r"(?i)Layout$",
    r"(?i)Sidebar$",
    r"(?i)NavBar$",
    r"(?i)ToolBar$",
    // LightSpeed stored procedures (the local DB layer, not the wire protocol).
    r"^LS[A-Z]",
    // Other Meta surfaces sharing the bundle.
    r"^Ads[A-Z]",
    r"^Billing[A-Z]",
    r"^Comet[A-Z]",
    r"^FB[A-Z]",
    r"^FDS[A-Z]",
    r"^Gemini[A-Z]",
    r"^IG[A-Z]",
    r"^Flows[A-Z]",
    r"^Mercury[A-Z]",
    r"^Messenger[A-Z]",
    r"^Profile[A-Z]",
    r"^Video[A-Z]",
    r"^VCE[A-Z]",
    r"^Cloud[A-Z]",
    r"^Lead[A-Z]",
    r"^Links[A-Z]",
    r"^Oxsights[A-Z]",
    r"^ReStore[A-Z]",
    r"^Shared[A-Z]",
    r"^Base64$",
    r"^Image[A-Z]",
    r"^EB[A-Z]",
    r"^SDK[A-Z]",
    r"^RST[A-Z]",
    r"^MSG[A-Z]",
    r"^Games[A-Z]",
    r"^Business[A-Z]",
    r"^LWI[A-Z]",
    r"^Get[A-Z]",
    r"Validation",
    r"^WDS[A-Z]",
    r"^Work[A-Z]",
    r"^Worm[A-Z]",
    r"^XPlat[A-Z]",
    r"^Zapier",
    r"^blob",
    r"^create[A-Z]",
    r"^dsp[A-Z]",
    r"^get[A-Z]",
    r"^json",
    r"^meta-",
    r"^path-",
    r"^pipeline-",
    r"^prop-types",
    r"\.stylex$",
    r"^hyperion",
    r"^promise",
    r"^private",
    r"^Webm",
    // Desktop shell plumbing, not protocol.
    r"^Windows",
    r"^WAWebWindows",
    r"^WAWebuse[A-Z]",
    // `WaWeb` with a lowercase `a` is a different, non-protocol namespace from `WAWeb`.
    r"^WaWeb",
    r"^useChatJidFromThreadKey$",
    // Instagram / Reels / video-creation surfaces.
    r"^Ig[A-Z]",
    r"^Reels[A-Z]",
    r"^Showreel",
    r"ShowreelAsset",
    r"^VCK[A-Z]",
    r"^Vck[A-Z]",
    r"^MFC[A-Z]",
    r"^Page[A-Z]",
    r"^Srapolygon",
    r"^KineticText",
];

/// Dependencies whose presence or absence says nothing about the protocol.
const NOISE_DEPS: &[&str] = &[
    r"(?i)^react",
    r"(?i)^Promise$",
    r"(?i)^asyncToGeneratorRuntime$",
    r"(?i)^asyncGeneratorRuntime$",
    r"(?i)^fbt$",
    r"(?i)^stylex",
    r"(?i)^once$",
    r"(?i)^invariant$",
    r"(?i)^nullthrows$",
    r"(?i)^emptyFunction$",
    r"(?i)^performanceNow$",
    r"(?i)^ExecutionEnvironment$",
    r"(?i)^fbjs",
    r"(?i)^classnames$",
    r"(?i)^shallowEqual$",
    r"(?i)^warning$",
];

/// Source lines that are transpiler or bundler output. A module whose changed lines
/// are all of this kind was rebuilt, not rewritten.
const NOISE_CODE: &[&str] = &[
    r"asyncGeneratorRuntime",
    r"asyncToGeneratorRuntime",
    r"_asyncToGenerator",
    r"_asyncGenerator",
    r"__generator",
    r"__awaiter",
    r"regeneratorRuntime",
    r"babelHelpers",
    r"\.apply\(this,\s*arguments\)",
    r"^[\-\+]\s*return [a-z]\.apply\(",
    r"^[\-\+]\s*[a-z]\.apply\(this,$",
    r"^[\-\+]\s*arguments\)?$",
];

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// Every compiled-in filter, sorted by name.
pub fn all() -> Vec<Filter> {
    let mut v = vec![default(), all_modules(), protocol_only(), schemas(), wam()];
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// Look up a built-in by name.
pub fn get(name: &str) -> Option<Filter> {
    all().into_iter().find(|f| f.name == name)
}

/// The filter used when none is named.
pub fn default() -> Filter {
    Filter {
        name: "default".into(),
        description: "Protocol surface: drops UI, analytics event shapes, and the other Meta \
                      products sharing the bundle. Ported from ProtoCocktail's wa-diff-analyzer. \
                      Excludes .pb/.graphql/.proto modules, which are structured artifacts better \
                      read with the `schemas` filter or a dedicated extractor."
            .into(),
        hard_exclude: strings(HARD_EXCLUDE),
        include: strings(PROTOCOL),
        exclude: strings(UI_AND_UNRELATED),
        default_verdict: Verdict::Keep,
        exclude_dependents_of_excluded: true,
        noise_deps: strings(NOISE_DEPS),
        noise_code: strings(NOISE_CODE),
        builtin: true,
    }
}

/// No filtering at all — every module in the bundle.
pub fn all_modules() -> Filter {
    Filter {
        name: "all".into(),
        description: "No filtering: every module in the bundle. Expect ~100k modules and a \
                      correspondingly large diff."
            .into(),
        hard_exclude: vec![],
        include: vec![],
        exclude: vec![],
        default_verdict: Verdict::Keep,
        exclude_dependents_of_excluded: false,
        noise_deps: strings(NOISE_DEPS),
        noise_code: strings(NOISE_CODE),
        builtin: true,
    }
}

/// Allow-list: only modules matching the protocol patterns.
pub fn protocol_only() -> Filter {
    Filter {
        name: "protocol".into(),
        description: "Allow-list: only modules matching the protocol patterns. Much narrower than \
                      `default`, which keeps anything not positively identified as UI."
            .into(),
        hard_exclude: strings(HARD_EXCLUDE),
        include: strings(PROTOCOL),
        exclude: vec![],
        default_verdict: Verdict::Drop,
        exclude_dependents_of_excluded: false,
        noise_deps: strings(NOISE_DEPS),
        noise_code: strings(NOISE_CODE),
        builtin: true,
    }
}

/// Allow-list: the protobuf and GraphQL schema modules `default` hard-excludes.
pub fn schemas() -> Filter {
    Filter {
        name: "schemas".into(),
        description: "Allow-list: .pb / .proto / .graphql schema modules — where new protobuf \
                      fields and new persisted GraphQL operations appear. The complement of \
                      `default`'s hard exclusions."
            .into(),
        hard_exclude: vec![],
        include: strings(&[r"\.pb$", r"\.proto$", r"\.graphql$"]),
        exclude: vec![],
        default_verdict: Verdict::Drop,
        exclude_dependents_of_excluded: false,
        noise_deps: strings(NOISE_DEPS),
        noise_code: strings(NOISE_CODE),
        builtin: true,
    }
}

/// Allow-list: the WAM analytics surface.
pub fn wam() -> Filter {
    Filter {
        name: "wam".into(),
        description: "Allow-list: WAM analytics events, enums and globals. New fields on a WAM \
                      event are often the earliest visible trace of an unreleased feature."
            .into(),
        hard_exclude: vec![],
        include: strings(&[r"WamEvent$", r"^WAWebWamEnum", r"^WAWebWam", r"^WAWamEvent"]),
        exclude: vec![],
        default_verdict: Verdict::Drop,
        exclude_dependents_of_excluded: false,
        noise_deps: strings(NOISE_DEPS),
        noise_code: strings(NOISE_CODE),
        builtin: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::{Because, FilterSet};

    #[test]
    fn every_builtin_compiles() {
        for f in all() {
            let name = f.name.clone();
            FilterSet::compile(f).unwrap_or_else(|e| panic!("builtin {name} must compile: {e}"));
        }
    }

    #[test]
    fn builtins_are_marked_builtin_and_uniquely_named() {
        let mut names: Vec<String> = all().into_iter().map(|f| f.name).collect();
        let before = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), before, "built-in names must be unique");
        assert!(all().iter().all(|f| f.builtin));
    }

    #[test]
    fn default_keeps_protocol_and_drops_ui() {
        let f = FilterSet::compile(default()).unwrap();
        for keep in [
            "WAWebSendMsgStanza",
            "WASmaxInIqCallReceiptResponse",
            "WAWebHandleGroupNotification",
            "WANoiseHandshake",
            "WAWebE2EProtoParse",
        ] {
            assert_eq!(f.classify(keep).0, Verdict::Keep, "{keep} should be kept");
        }
        for drop in [
            "WAWebChatListItem.react",
            "CometButton",
            "useChatState",
            "IGStoriesTray",
            "WAWebMsgSendWamEvent",
            "LSUpdateThread",
        ] {
            assert_eq!(
                f.classify(drop).0,
                Verdict::Drop,
                "{drop} should be dropped"
            );
        }
    }

    #[test]
    fn protocol_patterns_beat_ui_patterns() {
        let f = FilterSet::compile(default()).unwrap();
        // Matches the generic `(?i)Api$`-adjacent and `Action$` exclusions, but is
        // protocol and must survive.
        assert_eq!(f.classify("WAWebSendMessageAction").0, Verdict::Keep);
        // ...whereas the same suffix on an unrelated namespace does not.
        assert_eq!(f.classify("CometFeedStoryAction").0, Verdict::Drop);
    }

    #[test]
    fn hard_exclusions_beat_protocol_patterns() {
        let f = FilterSet::compile(default()).unwrap();
        // `WAProto` is in the protocol list; the `.pb` suffix still wins.
        assert_eq!(f.classify("WAProtoE2E.pb").0, Verdict::Drop);
        // ...and the `schemas` filter is where it reappears.
        let s = FilterSet::compile(schemas()).unwrap();
        assert_eq!(s.classify("WAProtoE2E.pb").0, Verdict::Keep);
    }

    #[test]
    fn wid_and_lid_patterns_do_not_falsely_claim_width_or_slider() {
        let f = FilterSet::compile(default()).unwrap();
        // `Wid[^t]` and `[^S]Lid` are negated-class patterns guarding against a
        // false *inclusion*, which is the expensive kind of mistake here: an
        // include match also pins the module against dependency propagation, so one
        // over-broad pattern drags a whole UI subtree into every diff. (These names
        // are still kept by `default`, which keeps anything it cannot identify as
        // UI — but kept by default, not claimed as protocol.)
        for benign in ["WAWebChatWidth", "MediaSlider", "WAWebColumnWidthUtils"] {
            let (_, why) = f.classify(benign);
            assert!(
                !matches!(why, Because::Include { .. }),
                "{benign} must not be claimed as protocol, got {why:?}"
            );
        }
        for real in ["WAWebWidFactory", "WAWebLidMigrationUtils", "WAWebJidUtils"] {
            let (verdict, why) = f.classify(real);
            assert!(
                matches!(why, Because::Include { .. }),
                "{real} must be claimed as protocol, got {why:?}"
            );
            assert_eq!(verdict, Verdict::Keep);
        }
    }

    #[test]
    fn all_filter_keeps_everything() {
        let f = FilterSet::compile(all_modules()).unwrap();
        for name in ["WAWebChatListItem.react", "CometButton", "WAWebSendMsg"] {
            assert_eq!(f.classify(name).0, Verdict::Keep);
        }
    }

    #[test]
    fn protocol_filter_is_an_allow_list() {
        let f = FilterSet::compile(protocol_only()).unwrap();
        assert_eq!(f.classify("WANoiseHandshake").0, Verdict::Keep);
        // Not positively identified as protocol: dropped, where `default` keeps it.
        assert_eq!(f.classify("SomeUnknownHelper").0, Verdict::Drop);
        assert_eq!(
            FilterSet::compile(default())
                .unwrap()
                .classify("SomeUnknownHelper")
                .0,
            Verdict::Keep
        );
    }
}
