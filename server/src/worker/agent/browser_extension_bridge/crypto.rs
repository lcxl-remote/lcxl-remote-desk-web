//! WebCrypto-compatible mutually authenticated transport, with no plaintext fallback.
use aes_gcm::{
    Aes256Gcm, KeyInit, Nonce,
    aead::{Aead, Payload},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, &'static str>;
const VERSION: u16 = 2;
const MAX_PLAINTEXT: usize = 96 * 1024 * 1024;
const MAX_INBOUND: usize = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientHello {
    pub version: u16,
    pub client_nonce: String,
    pub extension_version: String,
    pub browser_version: String,
    pub profile_incarnation: String,
}

#[derive(Serialize)]
pub(super) struct Challenge {
    pub version: u16,
    pub server_nonce: String,
    pub device_id: String,
    pub os_session_id: String,
    pub server_proof: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClientProof {
    pub version: u16,
    pub client_proof: String,
}

pub(super) struct Handshake {
    transcript: Vec<u8>,
    hkdf: Hkdf<Sha256>,
}

fn nonce(value: &str) -> Result<()> {
    let bytes = STANDARD.decode(value).map_err(|_| "invalid nonce")?;
    if bytes.len() != 32 || STANDARD.encode(&bytes) != value {
        return Err("invalid nonce");
    }
    Ok(())
}

impl Handshake {
    pub(super) fn new(
        secret: &str,
        hello: &ClientHello,
        server_nonce: String,
        device_id: &str,
        os_session_id: &str,
    ) -> Result<(Self, Challenge)> {
        if hello.version != VERSION {
            return Err("unsupported bridge protocol");
        }
        nonce(&hello.client_nonce)?;
        nonce(&server_nonce)?;
        let mut transcript = Vec::new();
        for value in [
            "lcxl-browser-v2",
            &hello.client_nonce,
            &server_nonce,
            device_id,
            os_session_id,
            &hello.extension_version,
            &hello.browser_version,
            &hello.profile_incarnation,
        ] {
            if value.len() > 512 || value.is_empty() {
                return Err("invalid handshake field");
            }
            transcript.extend_from_slice(&(value.len() as u32).to_be_bytes());
            transcript.extend_from_slice(value.as_bytes());
        }
        let salt = Sha256::digest(&transcript);
        let handshake = Self {
            hkdf: Hkdf::<Sha256>::new(Some(&salt), secret.as_bytes()),
            transcript,
        };
        let server_proof = STANDARD.encode(handshake.proof("server-proof")?);
        let challenge = Challenge {
            version: VERSION,
            server_nonce,
            device_id: device_id.into(),
            os_session_id: os_session_id.into(),
            server_proof,
        };
        Ok((handshake, challenge))
    }

    fn key(&self, purpose: &str) -> Result<[u8; 32]> {
        let mut key = [0; 32];
        self.hkdf
            .expand(format!("lcxl-browser-v2/{purpose}").as_bytes(), &mut key)
            .map_err(|_| "key derivation failed")?;
        Ok(key)
    }

    fn proof(&self, direction: &str) -> Result<Vec<u8>> {
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key(direction)?)
            .map_err(|_| "invalid proof key")?;
        mac.update(&self.transcript);
        Ok(mac.finalize().into_bytes().to_vec())
    }

    pub(super) fn finish(self, proof: ClientProof) -> Result<Cipher> {
        if proof.version != VERSION {
            return Err("invalid client proof version");
        }
        let bytes = STANDARD
            .decode(&proof.client_proof)
            .map_err(|_| "invalid client proof")?;
        let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(&self.key("client-proof")?)
            .map_err(|_| "invalid proof key")?;
        mac.update(&self.transcript);
        mac.verify_slice(&bytes)
            .map_err(|_| "client authentication failed")?;
        Ok(Cipher {
            outgoing: Aes256Gcm::new_from_slice(&self.key("server-to-client")?)
                .map_err(|_| "invalid cipher key")?,
            incoming: Aes256Gcm::new_from_slice(&self.key("client-to-server")?)
                .map_err(|_| "invalid cipher key")?,
            transcript_hash: Sha256::digest(&self.transcript).into(),
            send_sequence: 0,
            receive_sequence: 0,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Frame {
    version: u16,
    sequence: u32,
    ciphertext: String,
}

pub(super) struct Cipher {
    outgoing: Aes256Gcm,
    incoming: Aes256Gcm,
    transcript_hash: [u8; 32],
    send_sequence: u32,
    receive_sequence: u32,
}

fn frame_nonce(sequence: u32) -> [u8; 12] {
    let mut nonce = [0; 12];
    nonce[8..].copy_from_slice(&sequence.to_be_bytes());
    nonce
}

impl Cipher {
    fn aad(&self, direction: u8, sequence: u32) -> Vec<u8> {
        let mut aad = self.transcript_hash.to_vec();
        aad.push(direction);
        aad.extend_from_slice(&sequence.to_be_bytes());
        aad
    }
    pub(super) fn seal(&mut self, plaintext: &str) -> Result<String> {
        if plaintext.len() > MAX_PLAINTEXT {
            return Err("outbound frame too large");
        }
        let sequence = self.send_sequence;
        self.send_sequence = sequence.checked_add(1).ok_or("send sequence exhausted")?;
        let nonce = frame_nonce(sequence);
        let aad = self.aad(0, sequence);
        let ciphertext = self
            .outgoing
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: plaintext.as_bytes(),
                    aad: &aad,
                },
            )
            .map_err(|_| "encryption failed")?;
        serde_json::to_string(&Frame {
            version: VERSION,
            sequence,
            ciphertext: STANDARD.encode(ciphertext),
        })
        .map_err(|_| "invalid frame")
    }
    pub(super) fn open(&mut self, frame: &str) -> Result<String> {
        if frame.len() > MAX_INBOUND * 2 {
            return Err("inbound frame too large");
        }
        let frame: Frame = serde_json::from_str(frame).map_err(|_| "invalid encrypted frame")?;
        if frame.version != VERSION || frame.sequence != self.receive_sequence {
            return Err("replayed or unordered frame");
        }
        let next = self
            .receive_sequence
            .checked_add(1)
            .ok_or("receive sequence exhausted")?;
        let ciphertext = STANDARD
            .decode(frame.ciphertext)
            .map_err(|_| "invalid ciphertext")?;
        if ciphertext.len() > MAX_INBOUND {
            return Err("inbound frame too large");
        }
        let nonce = frame_nonce(frame.sequence);
        let aad = self.aad(1, frame.sequence);
        let plaintext = self
            .incoming
            .decrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: &ciphertext,
                    aad: &aad,
                },
            )
            .map_err(|_| "frame authentication failed")?;
        self.receive_sequence = next;
        String::from_utf8(plaintext).map_err(|_| "invalid plaintext")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn handshake() -> Handshake {
        Handshake::new(
            "test-pairing-secret-with-enough-entropy",
            &ClientHello {
                version: 2,
                client_nonce: STANDARD.encode([1u8; 32]),
                extension_version: "0.1.0".into(),
                browser_version: "120.0".into(),
                profile_incarnation: "profile-test".into(),
            },
            STANDARD.encode([2u8; 32]),
            "device-test",
            "session-test",
        )
        .unwrap()
        .0
    }
    #[test]
    fn server_proof_cannot_be_reflected_as_client_proof() {
        let state = handshake();
        let reflected = STANDARD.encode(state.proof("server-proof").unwrap());
        assert!(
            state
                .finish(ClientProof {
                    version: 2,
                    client_proof: reflected
                })
                .is_err()
        );
    }
    #[test]
    fn ciphertext_cannot_be_reflected_between_directions() {
        let state = handshake();
        let proof = STANDARD.encode(state.proof("client-proof").unwrap());
        let mut cipher = state
            .finish(ClientProof {
                version: 2,
                client_proof: proof,
            })
            .unwrap();
        let sealed = cipher.seal("private page data").unwrap();
        assert!(!sealed.contains("private page data"));
        assert!(cipher.open(&sealed).is_err());
        assert_eq!(cipher.receive_sequence, 0);
    }
    #[test]
    fn sequence_exhaustion_never_reuses_a_nonce() {
        let state = handshake();
        let proof = STANDARD.encode(state.proof("client-proof").unwrap());
        let mut cipher = state
            .finish(ClientProof {
                version: 2,
                client_proof: proof,
            })
            .unwrap();
        cipher.send_sequence = u32::MAX;
        assert!(cipher.seal("command").is_err());
    }
}

#[cfg(test)]
mod interop;
