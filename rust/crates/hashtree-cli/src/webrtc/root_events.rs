use nostr::{nips::nip19::FromBech32, Alphabet, Event, Filter, Kind, PublicKey, SingleLetterTag};

#[derive(Debug, Clone)]
pub struct PeerRootEvent {
    pub hash: String,
    pub key: Option<String>,
    pub encrypted_key: Option<String>,
    pub self_encrypted_key: Option<String>,
    pub event_id: String,
    pub created_at: u64,
    pub peer_id: String,
}

pub const HASHTREE_KIND: u16 = 30078;
pub const HASHTREE_LABEL: &str = "hashtree";

pub fn build_root_filter(owner_pubkey: &str, tree_name: &str) -> Option<Filter> {
    let author = PublicKey::from_hex(owner_pubkey)
        .or_else(|_| PublicKey::from_bech32(owner_pubkey))
        .ok()?;
    Some(
        Filter::new()
            .kind(Kind::Custom(HASHTREE_KIND))
            .author(author)
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::D),
                vec![tree_name.to_string()],
            )
            .custom_tag(
                SingleLetterTag::lowercase(Alphabet::L),
                vec![HASHTREE_LABEL.to_string()],
            )
            .limit(50),
    )
}

pub fn hashtree_event_identifier(event: &Event) -> Option<String> {
    event.tags.iter().find_map(|tag| {
        let slice = tag.as_slice();
        if slice.len() >= 2 && slice[0].as_str() == "d" {
            Some(slice[1].to_string())
        } else {
            None
        }
    })
}

pub fn is_hashtree_labeled_event(event: &Event) -> bool {
    event.tags.iter().any(|tag| {
        let slice = tag.as_slice();
        slice.len() >= 2 && slice[0].as_str() == "l" && slice[1].as_str() == HASHTREE_LABEL
    })
}

pub fn pick_latest_event<'a, I>(events: I) -> Option<&'a Event>
where
    I: IntoIterator<Item = &'a Event>,
{
    events.into_iter().max_by(|a, b| {
        let ordering = a.created_at.cmp(&b.created_at);
        if ordering == std::cmp::Ordering::Equal {
            a.id.cmp(&b.id)
        } else {
            ordering
        }
    })
}

pub fn root_event_from_peer(
    event: &Event,
    peer_id: &str,
    tree_name: &str,
) -> Option<PeerRootEvent> {
    if hashtree_event_identifier(event).as_deref() != Some(tree_name)
        || !is_hashtree_labeled_event(event)
    {
        return None;
    }

    let mut key = None;
    let mut encrypted_key = None;
    let mut self_encrypted_key = None;
    let mut hash_tag = None;

    for tag in &event.tags {
        let slice = tag.as_slice();
        if slice.len() < 2 {
            continue;
        }
        match slice[0].as_str() {
            "hash" => hash_tag = Some(slice[1].to_string()),
            "key" => key = Some(slice[1].to_string()),
            "encryptedKey" => encrypted_key = Some(slice[1].to_string()),
            "selfEncryptedKey" => self_encrypted_key = Some(slice[1].to_string()),
            _ => {}
        }
    }

    let hash = hash_tag.or_else(|| {
        if event.content.is_empty() {
            None
        } else {
            Some(event.content.clone())
        }
    })?;

    Some(PeerRootEvent {
        hash,
        key,
        encrypted_key,
        self_encrypted_key,
        event_id: event.id.to_hex(),
        created_at: event.created_at.as_u64(),
        peer_id: peer_id.to_string(),
    })
}
