//! Explicit Node.js/WebCrypto interoperability; no network or live browser.
use super::*;
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Stdio},
};

struct Peer {
    child: Child,
    output: BufReader<std::process::ChildStdout>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
impl Peer {
    fn exchange(&mut self, request: serde_json::Value) -> serde_json::Value {
        let input = self.child.stdin.as_mut().unwrap();
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        assert!(
            self.output.read_line(&mut line).unwrap() > 0,
            "WebCrypto peer exited"
        );
        serde_json::from_str(&line).unwrap()
    }
    fn value(&mut self, request: serde_json::Value) -> String {
        let response = self.exchange(request);
        assert_eq!(response["ok"], true, "{response}");
        response["result"].as_str().unwrap().into()
    }
}

#[test]
#[ignore = "requires Node.js 22; run this exact test under an external timeout"]
fn rust_and_actual_extension_webcrypto_interoperate() {
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../browser-extension/tests/bridge-crypto-peer.mjs");
    let mut child = Command::new("node")
        .arg(script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let output = BufReader::new(child.stdout.take().unwrap());
    let mut peer = Peer { child, output };
    let hello = peer.value(serde_json::json!({"op":"hello", "secret":"interop-test-secret",
        "metadata":{"extension_version":"1", "browser_version":"test", "profile_incarnation":"中文-profile"}}));
    let hello: ClientHello = serde_json::from_str(&hello).unwrap();
    let (handshake, challenge) = Handshake::new(
        "interop-test-secret",
        &hello,
        STANDARD.encode([9; 32]),
        "test-device",
        "c7",
    )
    .unwrap();
    let proof = peer.value(
        serde_json::json!({"op":"challenge", "text":serde_json::to_string(&challenge).unwrap()}),
    );
    let mut cipher = handshake
        .finish(serde_json::from_str(&proof).unwrap())
        .unwrap();
    for plaintext in ["页面内容 🧪", "second frame", ""] {
        let encrypted = cipher.seal(plaintext).unwrap();
        let decoded = peer.value(serde_json::json!({"op":"open", "text":encrypted}));
        assert_eq!(decoded, plaintext);
        let encrypted = peer.value(serde_json::json!({"op":"seal", "text":plaintext}));
        assert_eq!(cipher.open(&encrypted).unwrap(), plaintext);
        assert!(cipher.open(&encrypted).is_err(), "Rust accepted a replay");
    }
    let frame = cipher.seal("last frame").unwrap();
    assert_eq!(
        peer.value(serde_json::json!({"op":"open", "text":frame})),
        "last frame"
    );
    assert_eq!(
        peer.exchange(serde_json::json!({"op":"open", "text":frame}))["ok"],
        false
    );
    let next = cipher.seal("must not revive a failed cipher").unwrap();
    assert_eq!(
        peer.exchange(serde_json::json!({"op":"open", "text":next}))["ok"],
        false
    );
}
