//! Golden CBOR vector tests for the v1 wire format.
//!
//! Every core schema type is pinned to a fixed, hand-verified CBOR byte
//! string: `encode_payload(value) == GOLDEN` AND `decode_payload(GOLDEN)
//! == value`. Any accidental wire change (renamed field, reordered key,
//! changed tag, lost `skip_serializing_if`) fails these tests immediately
//! — this is the real wire-compatibility guarantee against the TS
//! `@earendil-works/pi-protocol` v1 codec.
//!
//! Field layouts were verified against ciborium's deterministic struct
//! encoding: structs/enums become definite-length CBOR maps, internally
//! tagged enums (`#[serde(tag = ...)]`) write the tag key FIRST, then the
//! fields in declaration order; `skip_serializing_if = "Option::is_none"`
//! keys are absent when None; integers use shortest-form CBOR ints; f64
//! cost fields are 8-byte IEEE754 doubles (0xfb prefix).
//!
//! To regenerate the vectors after an INTENTIONAL wire change:
//!
//! ```sh
//! cargo test -p tack-protocol --offline --test golden_vectors \
//!     -- --ignored generate_goldens --nocapture
//! ```
//!
//! and paste the printed `const` block over the one below (review the diff
//! byte by byte — that diff IS the wire-format change).
#![allow(clippy::unwrap_used)]

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::fmt::Debug;
use tack_protocol::schemas::*;
use tack_protocol::{decode_payload, encode_payload};

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn unhex(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "odd hex length");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Pin `value` to its golden bytes in both directions, and require the
/// decoded value to re-encode to the identical bytes (canonical form).
fn assert_golden<T>(value: &T, golden: &str)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let golden_bytes = unhex(golden);
    let encoded = encode_payload(value).unwrap();
    assert_eq!(
        encoded, golden_bytes,
        "encoding drifted from the golden vector\nvalue: {value:?}"
    );
    let decoded: T = decode_payload(&golden_bytes).unwrap();
    assert_eq!(&decoded, value, "golden no longer decodes to the value");
    let reencoded = encode_payload(&decoded).unwrap();
    assert_eq!(reencoded, golden_bytes, "decode→encode is not stable");
}

// ---------------------------------------------------------------------------
// Shared builders (single source of truth for tests AND the generator)
// ---------------------------------------------------------------------------

fn model_ref() -> ModelRef {
    ModelRef {
        provider: "anthropic".to_string(),
        id: "claude-x".to_string(),
    }
}

fn usage_with_reasoning() -> Usage {
    Usage {
        input: 100,
        output: 42,
        cache_read: 7,
        cache_write: 3,
        reasoning: Some(11),
        total_tokens: 163,
        cost: UsageCost {
            input: 0.5,
            output: 1.5,
            cache_read: 0.01,
            cache_write: 0.02,
            total: 2.03,
        },
    }
}

fn usage_without_reasoning() -> Usage {
    Usage {
        reasoning: None,
        ..usage_with_reasoning()
    }
}

fn session_metadata_full() -> SessionMetadata {
    SessionMetadata {
        id: "sess-1".to_string(),
        created_at: 1_700_000_000,
        updated_at: Some(1_700_000_100),
        parent_session_id: Some("sess-0".to_string()),
        session_name: Some("main".to_string()),
        cwd: Some("/work".to_string()),
    }
}

fn session_metadata_min() -> SessionMetadata {
    SessionMetadata {
        id: "sess-2".to_string(),
        created_at: 1_700_000_200,
        updated_at: None,
        parent_session_id: None,
        session_name: None,
        cwd: None,
    }
}

fn transcript_item_user() -> TranscriptItem {
    TranscriptItem::User {
        id: "msg-1".to_string(),
        content: vec![
            UserContent::Text {
                text: "hi".to_string(),
            },
            UserContent::Image {
                data: "aGVsbG8=".to_string(),
                mime_type: "image/png".to_string(),
            },
        ],
        timestamp: 1_700_000_010,
    }
}

fn transcript_item_assistant() -> TranscriptItem {
    TranscriptItem::Assistant {
        id: "msg-2".to_string(),
        content: vec![
            AssistantContent::Text {
                text: "answer".to_string(),
            },
            AssistantContent::Thinking {
                thinking: "hmm".to_string(),
                redacted: Some(true),
            },
            AssistantContent::ToolCall {
                tool_call_id: "tc-1".to_string(),
                tool_name: "bash".to_string(),
                input: json!({"command": "ls"}),
            },
        ],
        model: model_ref(),
        response_model: Some("claude-x-20250101".to_string()),
        usage: Some(usage_with_reasoning()),
        timestamp: 1_700_000_020,
        status: "complete".to_string(),
        stop_reason: Some("stop".to_string()),
        error_message: None,
    }
}

fn transcript_item_tool() -> TranscriptItem {
    TranscriptItem::Tool {
        id: "msg-3".to_string(),
        tool_call_id: "tc-1".to_string(),
        tool_name: "bash".to_string(),
        input: json!({"command": "ls"}),
        content: vec![UserContent::Text {
            text: "file.txt".to_string(),
        }],
        details: Some(json!({"exitCode": 0})),
        usage: None,
        timestamp: 1_700_000_030,
        status: "complete".to_string(),
        is_error: true,
    }
}

fn session_snapshot_min() -> SessionSnapshot {
    SessionSnapshot {
        id: "sess-1".to_string(),
        name: None,
        cwd: "/work".to_string(),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_100,
        phase: SessionPhase::Idle,
        model: model_ref(),
        thinking_level: ThinkingLevel::Medium,
        attached: true,
        locked: false,
        revision: 3,
        mode: None,
        transcript: vec![],
        queued_steer: vec![],
        queued_steer_count: 0,
    }
}

fn session_snapshot_full() -> SessionSnapshot {
    SessionSnapshot {
        name: Some("main".to_string()),
        phase: SessionPhase::Turn,
        thinking_level: ThinkingLevel::High,
        attached: true,
        locked: false,
        revision: 9,
        transcript: vec![transcript_item_user(), transcript_item_assistant()],
        queued_steer: vec![transcript_item_user()],
        queued_steer_count: 1,
        ..session_snapshot_min()
    }
}

fn server_snapshot_min() -> ServerSnapshot {
    ServerSnapshot {
        server_id: "srv-1".to_string(),
        protocol_version: PROTOCOL_VERSION,
        revision: 7,
        sessions: vec![],
        models: vec![],
    }
}

fn model_metadata() -> ModelMetadata {
    ModelMetadata {
        provider: "anthropic".to_string(),
        id: "claude-x".to_string(),
        name: "Claude X".to_string(),
        api: "anthropic-messages".to_string(),
        reasoning: true,
        input: vec!["text".to_string(), "image".to_string()],
        context_window: 200_000,
        max_tokens: 8192,
        cost: ModelCost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        },
        supported_thinking_levels: vec![
            ThinkingLevel::Off,
            ThinkingLevel::Low,
            ThinkingLevel::High,
        ],
        authenticated: true,
    }
}

fn server_snapshot_full() -> ServerSnapshot {
    ServerSnapshot {
        sessions: vec![session_metadata_full()],
        models: vec![model_metadata()],
        ..server_snapshot_min()
    }
}

// ---------------------------------------------------------------------------
// Golden vectors (generated; see header for regeneration instructions)
// ---------------------------------------------------------------------------
// GOLDEN_CONSTS

// ---------------------------------------------------------------------------
// Golden vectors (hex-encoded CBOR). Regenerate with the ignored
// `generate_goldens` test at the bottom of this file (see header docs).
// ---------------------------------------------------------------------------

const THINKING_LEVEL_OFF: &str = "636f6666";
const THINKING_LEVEL_MINIMAL: &str = "676d696e696d616c";
const THINKING_LEVEL_LOW: &str = "636c6f77";
const THINKING_LEVEL_MEDIUM: &str = "666d656469756d";
const THINKING_LEVEL_HIGH: &str = "6468696768";
const THINKING_LEVEL_XHIGH: &str = "657868696768";
const THINKING_LEVEL_MAX: &str = "636d6178";
const SESSION_PHASE_IDLE: &str = "6469646c65";
const SESSION_PHASE_TURN: &str = "647475726e";
const SESSION_PHASE_COMPACTION: &str = "6a636f6d70616374696f6e";
const SESSION_PHASE_BRANCH_SUMMARY: &str = "6e6272616e63685f73756d6d617279";
const SESSION_PHASE_RETRY: &str = "657265747279";
const ERROR_CODE_VERSION: &str = "6776657273696f6e";
const ERROR_CODE_AUTH: &str = "6461757468";
const ERROR_CODE_BUSY: &str = "6462757379";
const ERROR_CODE_SESSION_LOCKED: &str = "6e73657373696f6e5f6c6f636b6564";
const ERROR_CODE_NOT_FOUND: &str = "696e6f745f666f756e64";
const ERROR_CODE_INVALID_REQUEST: &str = "6f696e76616c69645f72657175657374";
const ERROR_CODE_NOT_IMPLEMENTED: &str = "6f6e6f745f696d706c656d656e746564";
const ERROR_CODE_INTERNAL_ERROR: &str = "6e696e7465726e616c5f6572726f72";
const CLIENT_HELLO_NO_TOKEN: &str = "a264747970656568656c6c6f6776657273696f6e01";
const CLIENT_HELLO_WITH_TOKEN: &str =
    "a364747970656568656c6c6f6776657273696f6e0165746f6b656e66733363726574";
const CLIENT_REQUEST_LIST: &str =
    "a364747970656772657175657374626964657265712d316772657175657374a167636f6d6d616e64646c697374";
const COMMAND_LIST: &str = "a167636f6d6d616e64646c697374";
const COMMAND_CREATE_EMPTY: &str = "a167636f6d6d616e6466637265617465";
const COMMAND_CREATE_FULL: &str = "a567636f6d6d616e646663726561746563637764652f776f726b646e616d65646d61696e656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d7468696e6b696e674c6576656c6468696768";
const COMMAND_ATTACH: &str = "a267636f6d6d616e64666174746163686973657373696f6e496466736573732d31";
const COMMAND_DETACH: &str = "a267636f6d6d616e64666465746163686973657373696f6e496466736573732d31";
const COMMAND_PROMPT: &str =
    "a367636f6d6d616e646670726f6d70746973657373696f6e496466736573732d3164746578746568656c6c6f";
const COMMAND_STEER: &str =
    "a367636f6d6d616e646573746565726973657373696f6e496466736573732d31647465787465666f637573";
const COMMAND_ABORT: &str = "a267636f6d6d616e646561626f72746973657373696f6e496466736573732d31";
const COMMAND_SET_MODEL: &str = "a367636f6d6d616e64697365745f6d6f64656c6973657373696f6e496466736573732d31656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d78";
const COMMAND_SET_THINKING: &str = "a367636f6d6d616e646c7365745f7468696e6b696e676973657373696f6e496466736573732d316d7468696e6b696e674c6576656c657868696768";
const COMMAND_RESULT_LIST: &str = "a267636f6d6d616e64646c6973746873657373696f6e7382a662696466736573732d31696372656174656441741a6553f100697570646174656441741a6553f1646f706172656e7453657373696f6e496466736573732d306b73657373696f6e4e616d65646d61696e63637764652f776f726ba262696466736573732d32696372656174656441741a6553f1c8";
const COMMAND_RESULT_DETACH: &str =
    "a267636f6d6d616e64666465746163686973657373696f6e496466736573732d31";
const COMMAND_RESULT_PROMPT: &str = "a267636f6d6d616e646670726f6d70746773657373696f6ead62696466736573732d3163637764652f776f726b696372656174656441741a6553f100697570646174656441741a6553f1646570686173656469646c65656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d7468696e6b696e674c6576656c666d656469756d686174746163686564f5666c6f636b6564f4687265766973696f6e036a7472616e736372697074806b717565756564537465657280707175657565645374656572436f756e7400";
const COMMAND_RESULT_CREATE: &str = "a267636f6d6d616e64666372656174656773657373696f6ead62696466736573732d3163637764652f776f726b696372656174656441741a6553f100697570646174656441741a6553f1646570686173656469646c65656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d7468696e6b696e674c6576656c666d656469756d686174746163686564f5666c6f636b6564f4687265766973696f6e036a7472616e736372697074806b717565756564537465657280707175657565645374656572436f756e7400";
const COMMAND_RESULT_SET_THINKING: &str = "a267636f6d6d616e646c7365745f7468696e6b696e676773657373696f6ead62696466736573732d3163637764652f776f726b696372656174656441741a6553f100697570646174656441741a6553f1646570686173656469646c65656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d7468696e6b696e674c6576656c666d656469756d686174746163686564f5666c6f636b6564f4687265766973696f6e036a7472616e736372697074806b717565756564537465657280707175657565645374656572436f756e7400";
const SERVER_HELLO: &str = "a464747970656568656c6c6f6776657273696f6e016c636f6e6e656374696f6e496466636f6e6e2d3168736e617073686f74a5687365727665724964657372762d316f70726f746f636f6c56657273696f6e01687265766973696f6e076873657373696f6e7380666d6f64656c7380";
const SERVER_HELLO_ERROR: &str = "a264747970656b68656c6c6f5f6572726f72656572726f72a364636f64656461757468676d6573736167656962616420746f6b656e6764657461696c73a16468696e7470757365202d2d617574682d746f6b656e";
const SERVER_RESPONSE_OK: &str = "a4647479706568726573706f6e7365626964657265712d31626f6bf566726573756c74a267636f6d6d616e64666465746163686973657373696f6e496466736573732d31";
const SERVER_RESPONSE_ERR: &str = "a4647479706568726573706f6e7365626964657265712d32626f6bf4656572726f72a264636f6465696e6f745f666f756e64676d6573736167656f6e6f20737563682073657373696f6e";
const SERVER_EVENT_SERVER_SNAPSHOT: &str = "a26474797065656576656e74656576656e74a264747970656f7365727665725f736e617073686f7468736e617073686f74a5687365727665724964657372762d316f70726f746f636f6c56657273696f6e01687265766973696f6e076873657373696f6e7381a662696466736573732d31696372656174656441741a6553f100697570646174656441741a6553f1646f706172656e7453657373696f6e496466736573732d306b73657373696f6e4e616d65646d61696e63637764652f776f726b666d6f64656c7381ab6870726f766964657269616e7468726f70696362696468636c617564652d78646e616d6568436c6175646520586361706972616e7468726f7069632d6d6573736167657369726561736f6e696e67f565696e70757482647465787465696d6167656d636f6e7465787457696e646f771a00030d40696d6178546f6b656e7319200064636f7374a465696e707574f94200666f7574707574f94b8069636163686552656164fb3fd33333333333336a63616368655772697465f9438077737570706f727465645468696e6b696e674c6576656c7383636f6666636c6f7764686967686d61757468656e74696361746564f5";
const SERVER_EVENT_SESSION_SNAPSHOT: &str = "a26474797065656576656e74656576656e74a264747970657073657373696f6e5f736e617073686f7468736e617073686f74ad62696466736573732d3163637764652f776f726b696372656174656441741a6553f100697570646174656441741a6553f1646570686173656469646c65656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d7468696e6b696e674c6576656c666d656469756d686174746163686564f5666c6f636b6564f4687265766973696f6e036a7472616e736372697074806b717565756564537465657280707175657565645374656572436f756e7400";
const SERVER_EVENT_SESSION_PROGRESS: &str = "a26474797065656576656e74656576656e74a364747970657073657373696f6e5f70726f67726573736973657373696f6e496466736573732d316870726f6772657373a564747970656f617373697374616e745f64656c7461696d6573736167654964656d73672d326c636f6e74656e74496e64657800646b696e6464746578746564656c74616348656c";
const SERVER_EVENT_SESSION_REMOVED: &str = "a26474797065656576656e74656576656e74a264747970656f73657373696f6e5f72656d6f7665646973657373696f6e496466736573732d31";
const TRANSCRIPT_ITEM_USER: &str = "a464726f6c656475736572626964656d73672d3167636f6e74656e7482a2647479706564746578746474657874626869a3647479706565696d616765646461746168614756736247383d686d696d655479706569696d6167652f706e676974696d657374616d701a6553f10a";
const TRANSCRIPT_ITEM_ASSISTANT: &str = "a964726f6c6569617373697374616e74626964656d73672d3267636f6e74656e7483a264747970656474657874647465787466616e73776572a36474797065687468696e6b696e67687468696e6b696e6763686d6d687265646163746564f5a4647479706568746f6f6c43616c6c6a746f6f6c43616c6c49646474632d3168746f6f6c4e616d65646261736865696e707574a167636f6d6d616e64626c73656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d726573706f6e73654d6f64656c71636c617564652d782d3230323530313031657573616765a765696e7075741864666f7574707574182a69636163686552656164076a636163686557726974650369726561736f6e696e670b6b746f74616c546f6b656e7318a364636f7374a565696e707574f93800666f7574707574f93e0069636163686552656164fb3f847ae147ae147b6a63616368655772697465fb3f947ae147ae147b65746f74616cfb40003d70a3d70a3d6974696d657374616d701a6553f1146673746174757368636f6d706c6574656a73746f70526561736f6e6473746f70";
const TRANSCRIPT_ITEM_TOOL: &str = "aa64726f6c6564746f6f6c626964656d73672d336a746f6f6c43616c6c49646474632d3168746f6f6c4e616d65646261736865696e707574a167636f6d6d616e64626c7367636f6e74656e7481a26474797065647465787464746578746866696c652e7478746764657461696c73a16865786974436f6465006974696d657374616d701a6553f11e6673746174757368636f6d706c6574656769734572726f72f5";
const PROGRESS_ITEM_STARTED: &str = "a264747970656c6974656d5f73746172746564646974656da464726f6c656475736572626964656d73672d3167636f6e74656e7482a2647479706564746578746474657874626869a3647479706565696d616765646461746168614756736247383d686d696d655479706569696d6167652f706e676974696d657374616d701a6553f10a";
const PROGRESS_ASSISTANT_DELTA: &str = "a564747970656f617373697374616e745f64656c7461696d6573736167654964656d73672d326c636f6e74656e74496e64657801646b696e64687468696e6b696e676564656c746163686d6d";
const PROGRESS_ITEM_UPDATED: &str = "a264747970656c6974656d5f75706461746564646974656daa64726f6c6564746f6f6c626964656d73672d336a746f6f6c43616c6c49646474632d3168746f6f6c4e616d65646261736865696e707574a167636f6d6d616e64626c7367636f6e74656e7481a26474797065647465787464746578746866696c652e7478746764657461696c73a16865786974436f6465006974696d657374616d701a6553f11e6673746174757368636f6d706c6574656769734572726f72f5";
const PROGRESS_ITEM_FINISHED: &str = "a264747970656d6974656d5f66696e6973686564646974656da964726f6c6569617373697374616e74626964656d73672d3267636f6e74656e7483a264747970656474657874647465787466616e73776572a36474797065687468696e6b696e67687468696e6b696e6763686d6d687265646163746564f5a4647479706568746f6f6c43616c6c6a746f6f6c43616c6c49646474632d3168746f6f6c4e616d65646261736865696e707574a167636f6d6d616e64626c73656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d726573706f6e73654d6f64656c71636c617564652d782d3230323530313031657573616765a765696e7075741864666f7574707574182a69636163686552656164076a636163686557726974650369726561736f6e696e670b6b746f74616c546f6b656e7318a364636f7374a565696e707574f93800666f7574707574f93e0069636163686552656164fb3f847ae147ae147b6a63616368655772697465fb3f947ae147ae147b65746f74616cfb40003d70a3d70a3d6974696d657374616d701a6553f1146673746174757368636f6d706c6574656a73746f70526561736f6e6473746f70";
const USAGE_WITH_REASONING: &str = "a765696e7075741864666f7574707574182a69636163686552656164076a636163686557726974650369726561736f6e696e670b6b746f74616c546f6b656e7318a364636f7374a565696e707574f93800666f7574707574f93e0069636163686552656164fb3f847ae147ae147b6a63616368655772697465fb3f947ae147ae147b65746f74616cfb40003d70a3d70a3d";
const USAGE_WITHOUT_REASONING: &str = "a665696e7075741864666f7574707574182a69636163686552656164076a63616368655772697465036b746f74616c546f6b656e7318a364636f7374a565696e707574f93800666f7574707574f93e0069636163686552656164fb3f847ae147ae147b6a63616368655772697465fb3f947ae147ae147b65746f74616cfb40003d70a3d70a3d";
const SESSION_SNAPSHOT_FULL: &str = "ae62696466736573732d31646e616d65646d61696e63637764652f776f726b696372656174656441741a6553f100697570646174656441741a6553f164657068617365647475726e656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d7468696e6b696e674c6576656c6468696768686174746163686564f5666c6f636b6564f4687265766973696f6e096a7472616e73637269707482a464726f6c656475736572626964656d73672d3167636f6e74656e7482a2647479706564746578746474657874626869a3647479706565696d616765646461746168614756736247383d686d696d655479706569696d6167652f706e676974696d657374616d701a6553f10aa964726f6c6569617373697374616e74626964656d73672d3267636f6e74656e7483a264747970656474657874647465787466616e73776572a36474797065687468696e6b696e67687468696e6b696e6763686d6d687265646163746564f5a4647479706568746f6f6c43616c6c6a746f6f6c43616c6c49646474632d3168746f6f6c4e616d65646261736865696e707574a167636f6d6d616e64626c73656d6f64656ca26870726f766964657269616e7468726f70696362696468636c617564652d786d726573706f6e73654d6f64656c71636c617564652d782d3230323530313031657573616765a765696e7075741864666f7574707574182a69636163686552656164076a636163686557726974650369726561736f6e696e670b6b746f74616c546f6b656e7318a364636f7374a565696e707574f93800666f7574707574f93e0069636163686552656164fb3f847ae147ae147b6a63616368655772697465fb3f947ae147ae147b65746f74616cfb40003d70a3d70a3d6974696d657374616d701a6553f1146673746174757368636f6d706c6574656a73746f70526561736f6e6473746f706b717565756564537465657281a464726f6c656475736572626964656d73672d3167636f6e74656e7482a2647479706564746578746474657874626869a3647479706565696d616765646461746168614756736247383d686d696d655479706569696d6167652f706e676974696d657374616d701a6553f10a707175657565645374656572436f756e7401";
const PROTOCOL_ERROR_NO_DETAILS: &str =
    "a264636f64656776657273696f6e676d65737361676574756e737570706f727465642070726f746f636f6c";

// ---------------------------------------------------------------------------
// Tests: small enums (all variants)
// ---------------------------------------------------------------------------

#[test]
fn golden_thinking_levels() {
    // Bare text strings, lowercase.
    assert_golden(&ThinkingLevel::Off, THINKING_LEVEL_OFF);
    assert_golden(&ThinkingLevel::Minimal, THINKING_LEVEL_MINIMAL);
    assert_golden(&ThinkingLevel::Low, THINKING_LEVEL_LOW);
    assert_golden(&ThinkingLevel::Medium, THINKING_LEVEL_MEDIUM);
    assert_golden(&ThinkingLevel::High, THINKING_LEVEL_HIGH);
    assert_golden(&ThinkingLevel::Xhigh, THINKING_LEVEL_XHIGH);
    assert_golden(&ThinkingLevel::Max, THINKING_LEVEL_MAX);
}

#[test]
fn golden_session_phases() {
    // Bare text strings, snake_case.
    assert_golden(&SessionPhase::Idle, SESSION_PHASE_IDLE);
    assert_golden(&SessionPhase::Turn, SESSION_PHASE_TURN);
    assert_golden(&SessionPhase::Compaction, SESSION_PHASE_COMPACTION);
    assert_golden(&SessionPhase::BranchSummary, SESSION_PHASE_BRANCH_SUMMARY);
    assert_golden(&SessionPhase::Retry, SESSION_PHASE_RETRY);
}

#[test]
fn golden_protocol_error_codes() {
    // Bare text strings, snake_case.
    assert_golden(&ProtocolErrorCode::Version, ERROR_CODE_VERSION);
    assert_golden(&ProtocolErrorCode::Auth, ERROR_CODE_AUTH);
    assert_golden(&ProtocolErrorCode::Busy, ERROR_CODE_BUSY);
    assert_golden(&ProtocolErrorCode::SessionLocked, ERROR_CODE_SESSION_LOCKED);
    assert_golden(&ProtocolErrorCode::NotFound, ERROR_CODE_NOT_FOUND);
    assert_golden(
        &ProtocolErrorCode::InvalidRequest,
        ERROR_CODE_INVALID_REQUEST,
    );
    assert_golden(
        &ProtocolErrorCode::NotImplemented,
        ERROR_CODE_NOT_IMPLEMENTED,
    );
    assert_golden(&ProtocolErrorCode::InternalError, ERROR_CODE_INTERNAL_ERROR);
}

// ---------------------------------------------------------------------------
// Tests: client → server messages
// ---------------------------------------------------------------------------

#[test]
fn golden_client_hello() {
    // {"type":"hello","version":1} — "token" key ABSENT when None.
    let value = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        token: None,
    };
    assert_golden(&value, CLIENT_HELLO_NO_TOKEN);

    let value = ClientMessage::Hello {
        version: PROTOCOL_VERSION,
        token: Some("s3cret".to_string()),
    };
    assert_golden(&value, CLIENT_HELLO_WITH_TOKEN);
}

#[test]
fn golden_client_request() {
    let value = ClientMessage::Request {
        id: "req-1".to_string(),
        request: Command::List,
    };
    assert_golden(&value, CLIENT_REQUEST_LIST);
}

#[test]
fn golden_commands() {
    // {"command":"list"} — unit variant is a one-key map.
    assert_golden(&Command::List, COMMAND_LIST);

    // All-optional create: absent keys when None vs all present.
    assert_golden(
        &Command::Create {
            cwd: None,
            name: None,
            model: None,
            thinking_level: None,
        },
        COMMAND_CREATE_EMPTY,
    );
    assert_golden(
        &Command::Create {
            cwd: Some("/work".to_string()),
            name: Some("main".to_string()),
            model: Some(model_ref()),
            thinking_level: Some(ThinkingLevel::High),
        },
        COMMAND_CREATE_FULL,
    );

    assert_golden(
        &Command::Attach {
            session_id: "sess-1".to_string(),
        },
        COMMAND_ATTACH,
    );
    assert_golden(
        &Command::Detach {
            session_id: "sess-1".to_string(),
        },
        COMMAND_DETACH,
    );
    assert_golden(
        &Command::Prompt {
            session_id: "sess-1".to_string(),
            text: "hello".to_string(),
        },
        COMMAND_PROMPT,
    );
    assert_golden(
        &Command::Steer {
            session_id: "sess-1".to_string(),
            text: "focus".to_string(),
        },
        COMMAND_STEER,
    );
    assert_golden(
        &Command::Abort {
            session_id: "sess-1".to_string(),
        },
        COMMAND_ABORT,
    );
    assert_golden(
        &Command::SetModel {
            session_id: "sess-1".to_string(),
            model: model_ref(),
        },
        COMMAND_SET_MODEL,
    );
    assert_golden(
        &Command::SetThinking {
            session_id: "sess-1".to_string(),
            thinking_level: ThinkingLevel::Xhigh,
        },
        COMMAND_SET_THINKING,
    );
}

// ---------------------------------------------------------------------------
// Tests: server → client messages
// ---------------------------------------------------------------------------

#[test]
fn golden_command_results() {
    // List carries session metadata; here one full + one minimal entry
    // (optional keys present vs absent).
    assert_golden(
        &CommandResult::List {
            sessions: vec![session_metadata_full(), session_metadata_min()],
        },
        COMMAND_RESULT_LIST,
    );
    assert_golden(
        &CommandResult::Detach {
            session_id: "sess-1".to_string(),
        },
        COMMAND_RESULT_DETACH,
    );
    // Snapshot-carrying results share one layout; pin one representative.
    assert_golden(
        &CommandResult::Prompt {
            session: session_snapshot_min(),
        },
        COMMAND_RESULT_PROMPT,
    );
    assert_golden(
        &CommandResult::Create {
            session: session_snapshot_min(),
        },
        COMMAND_RESULT_CREATE,
    );
    assert_golden(
        &CommandResult::SetThinking {
            session: session_snapshot_min(),
        },
        COMMAND_RESULT_SET_THINKING,
    );
}

#[test]
fn golden_server_hello() {
    let value = ServerMessage::Hello {
        version: PROTOCOL_VERSION,
        connection_id: "conn-1".to_string(),
        snapshot: server_snapshot_min(),
    };
    assert_golden(&value, SERVER_HELLO);
}

#[test]
fn golden_server_hello_error() {
    let value = ServerMessage::HelloError {
        error: ProtocolError {
            code: ProtocolErrorCode::Auth,
            message: "bad token".to_string(),
            details: Some(json!({"hint": "use --auth-token"})),
        },
    };
    assert_golden(&value, SERVER_HELLO_ERROR);
}

#[test]
fn golden_server_responses() {
    // ok=true, error key absent.
    let ok = ServerMessage::ok(
        "req-1",
        CommandResult::Detach {
            session_id: "sess-1".to_string(),
        },
    );
    assert_golden(&ok, SERVER_RESPONSE_OK);

    // ok=false, result key absent.
    let err = ServerMessage::err("req-2", ProtocolErrorCode::NotFound, "no such session");
    assert_golden(&err, SERVER_RESPONSE_ERR);
}

#[test]
fn golden_server_events() {
    assert_golden(
        &ServerMessage::Event {
            event: ServerEvent::ServerSnapshot {
                snapshot: server_snapshot_full(),
            },
        },
        SERVER_EVENT_SERVER_SNAPSHOT,
    );
    assert_golden(
        &ServerMessage::Event {
            event: ServerEvent::SessionSnapshot {
                snapshot: session_snapshot_min(),
            },
        },
        SERVER_EVENT_SESSION_SNAPSHOT,
    );
    assert_golden(
        &ServerMessage::Event {
            event: ServerEvent::SessionProgress {
                session_id: "sess-1".to_string(),
                progress: TranscriptProgress::AssistantDelta {
                    message_id: "msg-2".to_string(),
                    content_index: 0,
                    kind: "text".to_string(),
                    delta: "Hel".to_string(),
                },
            },
        },
        SERVER_EVENT_SESSION_PROGRESS,
    );
    assert_golden(
        &ServerMessage::Event {
            event: ServerEvent::SessionRemoved {
                session_id: "sess-1".to_string(),
            },
        },
        SERVER_EVENT_SESSION_REMOVED,
    );
}

// ---------------------------------------------------------------------------
// Tests: transcript model
// ---------------------------------------------------------------------------

#[test]
fn golden_transcript_items() {
    // User: content blocks carry their "type" tag first ("text"/"image",
    // image uses "mimeType").
    assert_golden(&transcript_item_user(), TRANSCRIPT_ITEM_USER);
    // Assistant: thinking with explicit redacted, toolCall with
    // "toolCallId"/"toolName"/"input"; responseModel + usage present,
    // errorMessage absent.
    assert_golden(&transcript_item_assistant(), TRANSCRIPT_ITEM_ASSISTANT);
    // Tool: "isError" key, details present, usage absent.
    assert_golden(&transcript_item_tool(), TRANSCRIPT_ITEM_TOOL);
}

#[test]
fn golden_transcript_progress() {
    assert_golden(
        &TranscriptProgress::ItemStarted {
            item: transcript_item_user(),
        },
        PROGRESS_ITEM_STARTED,
    );
    assert_golden(
        &TranscriptProgress::AssistantDelta {
            message_id: "msg-2".to_string(),
            content_index: 1,
            kind: "thinking".to_string(),
            delta: "hmm".to_string(),
        },
        PROGRESS_ASSISTANT_DELTA,
    );
    assert_golden(
        &TranscriptProgress::ItemUpdated {
            item: transcript_item_tool(),
        },
        PROGRESS_ITEM_UPDATED,
    );
    assert_golden(
        &TranscriptProgress::ItemFinished {
            item: transcript_item_assistant(),
        },
        PROGRESS_ITEM_FINISHED,
    );
}

#[test]
fn golden_usage_reasoning_key() {
    // "reasoning" present when Some …
    assert_golden(&usage_with_reasoning(), USAGE_WITH_REASONING);
    // … and ABSENT when None (skip_serializing_if).
    assert_golden(&usage_without_reasoning(), USAGE_WITHOUT_REASONING);
}

#[test]
fn golden_session_snapshot() {
    // phase / thinkingLevel / queuedSteer / queuedSteerCount and the full
    // transcript nesting in one vector.
    assert_golden(&session_snapshot_full(), SESSION_SNAPSHOT_FULL);
}

#[test]
fn golden_protocol_error_without_details() {
    // "details" key ABSENT when None.
    let value = ProtocolError {
        code: ProtocolErrorCode::Version,
        message: "unsupported protocol".to_string(),
        details: None,
    };
    assert_golden(&value, PROTOCOL_ERROR_NO_DETAILS);
}

// ---------------------------------------------------------------------------
// Golden regeneration (ignored; see header). Uses the SAME builders as the
// tests, so the printed consts always match what the tests assert.
// ---------------------------------------------------------------------------

#[test]
#[ignore = "golden generator, not a test"]
fn generate_goldens() {
    let vectors: Vec<(&str, Vec<u8>)> = vec![
        (
            "THINKING_LEVEL_OFF",
            encode_payload(&ThinkingLevel::Off).unwrap(),
        ),
        (
            "THINKING_LEVEL_MINIMAL",
            encode_payload(&ThinkingLevel::Minimal).unwrap(),
        ),
        (
            "THINKING_LEVEL_LOW",
            encode_payload(&ThinkingLevel::Low).unwrap(),
        ),
        (
            "THINKING_LEVEL_MEDIUM",
            encode_payload(&ThinkingLevel::Medium).unwrap(),
        ),
        (
            "THINKING_LEVEL_HIGH",
            encode_payload(&ThinkingLevel::High).unwrap(),
        ),
        (
            "THINKING_LEVEL_XHIGH",
            encode_payload(&ThinkingLevel::Xhigh).unwrap(),
        ),
        (
            "THINKING_LEVEL_MAX",
            encode_payload(&ThinkingLevel::Max).unwrap(),
        ),
        (
            "SESSION_PHASE_IDLE",
            encode_payload(&SessionPhase::Idle).unwrap(),
        ),
        (
            "SESSION_PHASE_TURN",
            encode_payload(&SessionPhase::Turn).unwrap(),
        ),
        (
            "SESSION_PHASE_COMPACTION",
            encode_payload(&SessionPhase::Compaction).unwrap(),
        ),
        (
            "SESSION_PHASE_BRANCH_SUMMARY",
            encode_payload(&SessionPhase::BranchSummary).unwrap(),
        ),
        (
            "SESSION_PHASE_RETRY",
            encode_payload(&SessionPhase::Retry).unwrap(),
        ),
        (
            "ERROR_CODE_VERSION",
            encode_payload(&ProtocolErrorCode::Version).unwrap(),
        ),
        (
            "ERROR_CODE_AUTH",
            encode_payload(&ProtocolErrorCode::Auth).unwrap(),
        ),
        (
            "ERROR_CODE_BUSY",
            encode_payload(&ProtocolErrorCode::Busy).unwrap(),
        ),
        (
            "ERROR_CODE_SESSION_LOCKED",
            encode_payload(&ProtocolErrorCode::SessionLocked).unwrap(),
        ),
        (
            "ERROR_CODE_NOT_FOUND",
            encode_payload(&ProtocolErrorCode::NotFound).unwrap(),
        ),
        (
            "ERROR_CODE_INVALID_REQUEST",
            encode_payload(&ProtocolErrorCode::InvalidRequest).unwrap(),
        ),
        (
            "ERROR_CODE_NOT_IMPLEMENTED",
            encode_payload(&ProtocolErrorCode::NotImplemented).unwrap(),
        ),
        (
            "ERROR_CODE_INTERNAL_ERROR",
            encode_payload(&ProtocolErrorCode::InternalError).unwrap(),
        ),
        (
            "CLIENT_HELLO_NO_TOKEN",
            encode_payload(&ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token: None,
            })
            .unwrap(),
        ),
        (
            "CLIENT_HELLO_WITH_TOKEN",
            encode_payload(&ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                token: Some("s3cret".to_string()),
            })
            .unwrap(),
        ),
        (
            "CLIENT_REQUEST_LIST",
            encode_payload(&ClientMessage::Request {
                id: "req-1".to_string(),
                request: Command::List,
            })
            .unwrap(),
        ),
        ("COMMAND_LIST", encode_payload(&Command::List).unwrap()),
        (
            "COMMAND_CREATE_EMPTY",
            encode_payload(&Command::Create {
                cwd: None,
                name: None,
                model: None,
                thinking_level: None,
            })
            .unwrap(),
        ),
        (
            "COMMAND_CREATE_FULL",
            encode_payload(&Command::Create {
                cwd: Some("/work".to_string()),
                name: Some("main".to_string()),
                model: Some(model_ref()),
                thinking_level: Some(ThinkingLevel::High),
            })
            .unwrap(),
        ),
        (
            "COMMAND_ATTACH",
            encode_payload(&Command::Attach {
                session_id: "sess-1".to_string(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_DETACH",
            encode_payload(&Command::Detach {
                session_id: "sess-1".to_string(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_PROMPT",
            encode_payload(&Command::Prompt {
                session_id: "sess-1".to_string(),
                text: "hello".to_string(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_STEER",
            encode_payload(&Command::Steer {
                session_id: "sess-1".to_string(),
                text: "focus".to_string(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_ABORT",
            encode_payload(&Command::Abort {
                session_id: "sess-1".to_string(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_SET_MODEL",
            encode_payload(&Command::SetModel {
                session_id: "sess-1".to_string(),
                model: model_ref(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_SET_THINKING",
            encode_payload(&Command::SetThinking {
                session_id: "sess-1".to_string(),
                thinking_level: ThinkingLevel::Xhigh,
            })
            .unwrap(),
        ),
        (
            "COMMAND_RESULT_LIST",
            encode_payload(&CommandResult::List {
                sessions: vec![session_metadata_full(), session_metadata_min()],
            })
            .unwrap(),
        ),
        (
            "COMMAND_RESULT_DETACH",
            encode_payload(&CommandResult::Detach {
                session_id: "sess-1".to_string(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_RESULT_PROMPT",
            encode_payload(&CommandResult::Prompt {
                session: session_snapshot_min(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_RESULT_CREATE",
            encode_payload(&CommandResult::Create {
                session: session_snapshot_min(),
            })
            .unwrap(),
        ),
        (
            "COMMAND_RESULT_SET_THINKING",
            encode_payload(&CommandResult::SetThinking {
                session: session_snapshot_min(),
            })
            .unwrap(),
        ),
        (
            "SERVER_HELLO",
            encode_payload(&ServerMessage::Hello {
                version: PROTOCOL_VERSION,
                connection_id: "conn-1".to_string(),
                snapshot: server_snapshot_min(),
            })
            .unwrap(),
        ),
        (
            "SERVER_HELLO_ERROR",
            encode_payload(&ServerMessage::HelloError {
                error: ProtocolError {
                    code: ProtocolErrorCode::Auth,
                    message: "bad token".to_string(),
                    details: Some(json!({"hint": "use --auth-token"})),
                },
            })
            .unwrap(),
        ),
        (
            "SERVER_RESPONSE_OK",
            encode_payload(&ServerMessage::ok(
                "req-1",
                CommandResult::Detach {
                    session_id: "sess-1".to_string(),
                },
            ))
            .unwrap(),
        ),
        (
            "SERVER_RESPONSE_ERR",
            encode_payload(&ServerMessage::err(
                "req-2",
                ProtocolErrorCode::NotFound,
                "no such session",
            ))
            .unwrap(),
        ),
        (
            "SERVER_EVENT_SERVER_SNAPSHOT",
            encode_payload(&ServerMessage::Event {
                event: ServerEvent::ServerSnapshot {
                    snapshot: server_snapshot_full(),
                },
            })
            .unwrap(),
        ),
        (
            "SERVER_EVENT_SESSION_SNAPSHOT",
            encode_payload(&ServerMessage::Event {
                event: ServerEvent::SessionSnapshot {
                    snapshot: session_snapshot_min(),
                },
            })
            .unwrap(),
        ),
        (
            "SERVER_EVENT_SESSION_PROGRESS",
            encode_payload(&ServerMessage::Event {
                event: ServerEvent::SessionProgress {
                    session_id: "sess-1".to_string(),
                    progress: TranscriptProgress::AssistantDelta {
                        message_id: "msg-2".to_string(),
                        content_index: 0,
                        kind: "text".to_string(),
                        delta: "Hel".to_string(),
                    },
                },
            })
            .unwrap(),
        ),
        (
            "SERVER_EVENT_SESSION_REMOVED",
            encode_payload(&ServerMessage::Event {
                event: ServerEvent::SessionRemoved {
                    session_id: "sess-1".to_string(),
                },
            })
            .unwrap(),
        ),
        (
            "TRANSCRIPT_ITEM_USER",
            encode_payload(&transcript_item_user()).unwrap(),
        ),
        (
            "TRANSCRIPT_ITEM_ASSISTANT",
            encode_payload(&transcript_item_assistant()).unwrap(),
        ),
        (
            "TRANSCRIPT_ITEM_TOOL",
            encode_payload(&transcript_item_tool()).unwrap(),
        ),
        (
            "PROGRESS_ITEM_STARTED",
            encode_payload(&TranscriptProgress::ItemStarted {
                item: transcript_item_user(),
            })
            .unwrap(),
        ),
        (
            "PROGRESS_ASSISTANT_DELTA",
            encode_payload(&TranscriptProgress::AssistantDelta {
                message_id: "msg-2".to_string(),
                content_index: 1,
                kind: "thinking".to_string(),
                delta: "hmm".to_string(),
            })
            .unwrap(),
        ),
        (
            "PROGRESS_ITEM_UPDATED",
            encode_payload(&TranscriptProgress::ItemUpdated {
                item: transcript_item_tool(),
            })
            .unwrap(),
        ),
        (
            "PROGRESS_ITEM_FINISHED",
            encode_payload(&TranscriptProgress::ItemFinished {
                item: transcript_item_assistant(),
            })
            .unwrap(),
        ),
        (
            "USAGE_WITH_REASONING",
            encode_payload(&usage_with_reasoning()).unwrap(),
        ),
        (
            "USAGE_WITHOUT_REASONING",
            encode_payload(&usage_without_reasoning()).unwrap(),
        ),
        (
            "SESSION_SNAPSHOT_FULL",
            encode_payload(&session_snapshot_full()).unwrap(),
        ),
        (
            "PROTOCOL_ERROR_NO_DETAILS",
            encode_payload(&ProtocolError {
                code: ProtocolErrorCode::Version,
                message: "unsupported protocol".to_string(),
                details: None,
            })
            .unwrap(),
        ),
    ];
    println!("// --- generated golden consts ---");
    for (name, bytes) in vectors {
        println!("const {name}: &str =\n    \"{}\";", hex(&bytes));
    }
}
