import { writable, type Readable } from 'svelte/store';
import { SimplePool, nip19 } from 'nostr-tools';
import { minidenticon } from 'minidenticons';
import { animalName } from './animalName';
import { generateProxyUrlAsync } from './imgproxy';
import { distributedOwner } from './apps';
import { getDaemonNetworkSettings, getHtreeServerUrl } from './tauri';

const pool = new SimplePool();
const DEFAULT_PROFILE_RELAYS = [
  'wss://relay.damus.io',
  'wss://relay.snort.social',
  'wss://relay.primal.net',
];
const OWNER_AVATAR_SIZE = 48;
const HEX_PUBKEY_RE = /^[0-9a-f]{64}$/i;

export interface AddressOwnerProfile {
  pubkey: string;
  name?: string;
  display_name?: string;
  username?: string;
  picture?: string;
  nip05?: string;
}

export interface AddressOwnerIdentity {
  host: string;
  pubkey: string | null;
  name: string;
  profileUrl: string;
  avatarUrl: string;
  fallbackAvatarUrl: string;
  showBadge: boolean;
  isFallbackName: boolean;
}

declare global {
  interface Window {
    __irisAddressOwnerProfiles?: Record<string, Partial<AddressOwnerProfile>>;
  }
}

const ownerStores = new Map<string, ReturnType<typeof writable<AddressOwnerIdentity>>>();
const ownerSnapshots = new Map<string, AddressOwnerIdentity>();
const pendingOwnerLoads = new Map<string, Promise<void>>();
let cachedLocalRelayUrl: string | null | undefined;
let cachedFallbackRelayUrls: string[] | null = null;

function uniqueStrings(values: Array<string | null | undefined>): string[] {
  const seen = new Set<string>();
  const output: string[] = [];
  for (const value of values) {
    const normalized = typeof value === 'string' ? value.trim().replace(/\/+$/, '') : '';
    if (!normalized || seen.has(normalized)) continue;
    seen.add(normalized);
    output.push(normalized);
  }
  return output;
}

function ownerPubkey(host: string): string | null {
  if (!host || host === 'self') return null;
  if (HEX_PUBKEY_RE.test(host)) return host.toLowerCase();
  if (!host.startsWith('npub1')) return null;

  try {
    const decoded = nip19.decode(host);
    return typeof decoded.data === 'string' ? decoded.data : null;
  } catch {
    return null;
  }
}

function ownerFallbackName(host: string): string {
  if (host === 'self') return 'You';
  return animalName(ownerPubkey(host) ?? host);
}

function ownerFallbackAvatarUrl(host: string): string {
  const seed = ownerPubkey(host) ?? host;
  return `data:image/svg+xml;utf8,${encodeURIComponent(minidenticon(seed, 50, 50))}`;
}

function ownerBadge(host: string, pubkey: string | null): boolean {
  return host === distributedOwner || pubkey === ownerPubkey(distributedOwner);
}

function ownerNameFromProfile(profile?: Partial<AddressOwnerProfile> | null): string | undefined {
  if (!profile) return undefined;
  return profile.display_name
    || profile.name
    || profile.username
    || (profile.nip05 ? profile.nip05.split('@')[0] : undefined);
}

function buildOwnerIdentity(host: string, profile?: Partial<AddressOwnerProfile> | null): AddressOwnerIdentity {
  const pubkey = ownerPubkey(host);
  const fallbackAvatarUrl = ownerFallbackAvatarUrl(host);
  const profileName = ownerNameFromProfile(profile);
  return {
    host,
    pubkey,
    name: profileName || ownerFallbackName(host),
    profileUrl: ownerProfileUrl(host),
    avatarUrl: profile?.picture || fallbackAvatarUrl,
    fallbackAvatarUrl,
    showBadge: ownerBadge(host, pubkey),
    isFallbackName: !profileName,
  };
}

function parseProfileContent(content: string, pubkey: string): AddressOwnerProfile | null {
  try {
    const parsed = JSON.parse(content);
    if (!parsed || typeof parsed !== 'object') return null;
    return {
      pubkey,
      name: typeof parsed.name === 'string' ? parsed.name : undefined,
      display_name: typeof parsed.display_name === 'string' ? parsed.display_name : undefined,
      username: typeof parsed.username === 'string' ? parsed.username : undefined,
      picture: typeof parsed.picture === 'string' ? parsed.picture : undefined,
      nip05: typeof parsed.nip05 === 'string' ? parsed.nip05 : undefined,
    };
  } catch {
    return null;
  }
}

function testProfile(host: string, pubkey: string | null): Partial<AddressOwnerProfile> | null {
  if (typeof window === 'undefined') return null;
  const fixtures = window.__irisAddressOwnerProfiles;
  if (!fixtures) return null;
  const profile = fixtures[host] || (pubkey ? fixtures[pubkey] : undefined);
  return profile ?? null;
}

async function loadLocalRelayUrl(): Promise<string | null> {
  if (cachedLocalRelayUrl !== undefined) return cachedLocalRelayUrl;
  try {
    const serverUrl = await getHtreeServerUrl();
    const relayUrl = new URL(serverUrl);
    if (relayUrl.protocol === 'http:') {
      relayUrl.protocol = 'ws:';
    } else if (relayUrl.protocol === 'https:') {
      relayUrl.protocol = 'wss:';
    } else {
      cachedLocalRelayUrl = null;
      return cachedLocalRelayUrl;
    }
    relayUrl.pathname = '/ws';
    relayUrl.search = '';
    relayUrl.hash = '';
    cachedLocalRelayUrl = relayUrl.toString().replace(/\/+$/, '');
  } catch {
    cachedLocalRelayUrl = null;
  }
  return cachedLocalRelayUrl;
}

async function loadFallbackRelayUrls(localRelayUrl: string | null): Promise<string[]> {
  if (cachedFallbackRelayUrls) return cachedFallbackRelayUrls;

  try {
    const settings = await getDaemonNetworkSettings();
    if (settings.nostrRelaysEnabled) {
      cachedFallbackRelayUrls = uniqueStrings([
        ...settings.relayUrls,
        ...DEFAULT_PROFILE_RELAYS,
      ]).filter((relay) => relay !== localRelayUrl);
      return cachedFallbackRelayUrls;
    }
  } catch {
    // Fall back to static relays below.
  }

  cachedFallbackRelayUrls = uniqueStrings(DEFAULT_PROFILE_RELAYS).filter((relay) => relay !== localRelayUrl);
  return cachedFallbackRelayUrls;
}

async function fetchProfileFromRelays(relays: string[], pubkey: string): Promise<AddressOwnerProfile | null> {
  if (relays.length === 0) return null;

  try {
    const event = await pool.get(
      relays,
      { kinds: [0], authors: [pubkey] },
      { maxWait: 4000 },
    );
    if (!event?.content) return null;
    return parseProfileContent(event.content, pubkey);
  } catch {
    return null;
  }
}

async function fetchOwnerProfile(host: string): Promise<AddressOwnerProfile | null> {
  const pubkey = ownerPubkey(host);
  if (!pubkey) return null;

  const mocked = testProfile(host, pubkey);
  if (mocked) {
    return {
      pubkey,
      name: mocked.name,
      display_name: mocked.display_name,
      username: mocked.username,
      picture: mocked.picture,
      nip05: mocked.nip05,
    };
  }

  const localRelayUrl = await loadLocalRelayUrl();
  if (localRelayUrl) {
    const localProfile = await fetchProfileFromRelays([localRelayUrl], pubkey);
    if (localProfile) return localProfile;
  }

  const fallbackRelays = await loadFallbackRelayUrls(localRelayUrl);
  return fetchProfileFromRelays(fallbackRelays, pubkey);
}

function ownerStore(host: string) {
  let store = ownerStores.get(host);
  if (!store) {
    const snapshot = ownerSnapshots.get(host) ?? buildOwnerIdentity(host);
    ownerSnapshots.set(host, snapshot);
    store = writable(snapshot);
    ownerStores.set(host, store);
    void refreshAddressOwner(host);
  }
  return store;
}

async function refreshAddressOwner(host: string): Promise<void> {
  const pending = pendingOwnerLoads.get(host);
  if (pending) {
    await pending;
    return;
  }

  const load = (async () => {
    const profile = await fetchOwnerProfile(host);
    if (!profile) return;

    const store = ownerStore(host);
    const nextOwner = buildOwnerIdentity(host, profile);
    ownerSnapshots.set(host, nextOwner);
    store.set(nextOwner);

    if (!profile.picture) return;

    try {
      const proxiedAvatarUrl = await generateProxyUrlAsync(profile.picture, {
        width: OWNER_AVATAR_SIZE,
        height: OWNER_AVATAR_SIZE,
        square: true,
      });
      if (!proxiedAvatarUrl || proxiedAvatarUrl === nextOwner.avatarUrl) return;
      const proxiedOwner = {
        ...nextOwner,
        avatarUrl: proxiedAvatarUrl,
      };
      ownerSnapshots.set(host, proxiedOwner);
      store.set(proxiedOwner);
    } catch {
      // Keep the original picture URL if proxy generation fails.
    }
  })().finally(() => {
    pendingOwnerLoads.delete(host);
  });

  pendingOwnerLoads.set(host, load);
  await load;
}

export function ownerDisplayName(host: string): string {
  return describeAddressOwner(host).name;
}

export function ownerProfileUrl(host: string): string {
  return `htree://${distributedOwner}/files/index.html#/${encodeURIComponent(host)}/profile`;
}

export function describeAddressOwner(host: string): AddressOwnerIdentity {
  return ownerSnapshots.get(host) ?? buildOwnerIdentity(host);
}

export function createAddressOwnerStore(host: string): Readable<AddressOwnerIdentity> {
  const store = ownerStore(host);
  return {
    subscribe: store.subscribe,
  };
}
