use anyhow::Context;
use bytes::{Bytes, BytesMut};
use nostr::{Keys, SecretKey};

const BINARY_FORMAT_VERSION: u64 = 2;
const CHUNK_SIZE: usize = 16 * 1024;

fn main() -> anyhow::Result<()> {
    let out_path = std::env::args()
        .nth(1)
        .context("Usage: socialgraph-snapshot-fixture <output-path>")?;

    let root_keys = keys_from_byte(1)?;
    let bob_keys = keys_from_byte(2)?;
    let carol_keys = keys_from_byte(3)?;
    let dave_keys = keys_from_byte(4)?;

    let root_pk = root_keys.public_key().to_bytes();
    let bob_pk = bob_keys.public_key().to_bytes();
    let carol_pk = carol_keys.public_key().to_bytes();
    let dave_pk = dave_keys.public_key().to_bytes();

    let root_follow_ts = 1_700_000_111;
    let bob_follow_ts = 1_700_000_222;
    let root_mute_ts = 1_700_000_333;

    // Generate a minimal snapshot binary directly.
    //
    // This keeps the TS test deterministic and avoids needing a live social graph
    // store in the test runner environment.
    let used_order = vec![root_pk, bob_pk, carol_pk, dave_pk];
    let follow_lists = vec![
        SnapshotList {
            owner: root_pk,
            created_at: root_follow_ts,
            targets: vec![bob_pk],
        },
        SnapshotList {
            owner: bob_pk,
            created_at: bob_follow_ts,
            targets: vec![carol_pk],
        },
    ];
    let mute_lists = vec![SnapshotList {
        owner: root_pk,
        created_at: root_mute_ts,
        targets: vec![dave_pk],
    }];

    let chunks = encode_snapshot_chunks(&used_order, &follow_lists, &mute_lists);

    let data = flatten_chunks(chunks);
    std::fs::write(&out_path, data).context("write snapshot")?;
    Ok(())
}

#[derive(Debug, Clone)]
struct SnapshotList {
    owner: [u8; 32],
    created_at: u64,
    targets: Vec<[u8; 32]>,
}

fn keys_from_byte(byte: u8) -> anyhow::Result<Keys> {
    let mut sk = [0u8; 32];
    sk.fill(byte);
    let secret = SecretKey::from_slice(&sk)?;
    Ok(Keys::new(secret))
}

fn encode_snapshot_chunks(
    used_order: &[[u8; 32]],
    follows: &[SnapshotList],
    mutes: &[SnapshotList],
) -> Vec<Bytes> {
    use std::collections::HashMap;

    let mut id_map: HashMap<[u8; 32], u32> = HashMap::new();
    for (idx, pk) in used_order.iter().enumerate() {
        id_map.insert(*pk, idx as u32);
    }

    let mut writer = ChunkWriter::new();
    writer.write_varint(BINARY_FORMAT_VERSION);

    writer.write_varint(used_order.len() as u64);
    for (idx, pk) in used_order.iter().enumerate() {
        writer.write_bytes(pk);
        writer.write_varint(idx as u64);
    }

    writer.write_varint(follows.len() as u64);
    for list in follows {
        let owner_id = id_map.get(&list.owner).copied().unwrap_or_default();
        writer.write_varint(owner_id as u64);
        writer.write_varint(list.created_at);
        writer.write_varint(list.targets.len() as u64);
        for target in &list.targets {
            let target_id = id_map.get(target).copied().unwrap_or_default();
            writer.write_varint(target_id as u64);
        }
    }

    writer.write_varint(mutes.len() as u64);
    for list in mutes {
        let owner_id = id_map.get(&list.owner).copied().unwrap_or_default();
        writer.write_varint(owner_id as u64);
        writer.write_varint(list.created_at);
        writer.write_varint(list.targets.len() as u64);
        for target in &list.targets {
            let target_id = id_map.get(target).copied().unwrap_or_default();
            writer.write_varint(target_id as u64);
        }
    }

    writer.finish()
}

struct ChunkWriter {
    buf: BytesMut,
    chunks: Vec<Bytes>,
}

impl ChunkWriter {
    fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(CHUNK_SIZE),
            chunks: Vec::new(),
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let remaining = CHUNK_SIZE - self.buf.len();
            if remaining == 0 {
                self.flush();
                continue;
            }
            let to_write = remaining.min(bytes.len() - offset);
            self.buf
                .extend_from_slice(&bytes[offset..offset + to_write]);
            offset += to_write;
        }
    }

    fn write_varint(&mut self, mut value: u64) {
        while value >= 0x80 {
            let byte = ((value as u8) & 0x7f) | 0x80;
            self.write_bytes(&[byte]);
            value >>= 7;
        }
        self.write_bytes(&[(value as u8) & 0x7f]);
    }

    fn flush(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let chunk = self.buf.split().freeze();
        self.chunks.push(chunk);
    }

    fn finish(mut self) -> Vec<Bytes> {
        self.flush();
        self.chunks
    }
}

fn flatten_chunks(chunks: Vec<Bytes>) -> Vec<u8> {
    let total = chunks.iter().map(|c| c.len()).sum::<usize>();
    let mut out = Vec::with_capacity(total);
    for chunk in chunks {
        out.extend_from_slice(&chunk);
    }
    out
}
