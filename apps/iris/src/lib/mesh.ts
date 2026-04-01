export type MeshPeerState = 'connected' | 'disconnected';

export interface MeshPeerInfo {
  id: string;
  peerId: string;
  pubkey: string;
  state: MeshPeerState;
  pool: 'follows' | 'others';
  bytesSent: number;
  bytesReceived: number;
  transport: string;
  signalPaths: string[];
}

export interface BluetoothReceivedEventInfo {
  eventId: string;
  pubkey: string;
  kind: number;
  createdAt: number;
  receivedAt: number;
  peerId: string | null;
  cidValues: string[];
}

export interface MeshTotals {
  totalBytesSent: number;
  totalBytesReceived: number;
}

export interface MeshHistoryCursor extends MeshTotals {
  timestamp: number;
}

export interface MeshBandwidthHistoryPoint extends MeshTotals {
  timestamp: number;
  uploadBps: number;
  downloadBps: number;
}

export interface DaemonMeshStatus {
  enabled: boolean;
  totalPeers: number;
  connected: number;
  withDataChannel: number;
  transportCounts: Record<string, number>;
  totalBytesSent: number;
  totalBytesReceived: number;
  peers: MeshPeerInfo[];
  bluetoothReceivedEvents: BluetoothReceivedEventInfo[];
  blossomServers: number;
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value !== null && typeof value === 'object' ? value as Record<string, unknown> : null;
}

function readNumber(value: unknown): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : 0;
}

function readString(value: unknown, fallback = ''): string {
  return typeof value === 'string' ? value : fallback;
}

function readStringArray(value: unknown): string[] {
  return Array.isArray(value)
    ? value.filter((entry): entry is string => typeof entry === 'string')
    : [];
}

function normalizePool(pool: string): 'follows' | 'others' {
  return pool.toLowerCase() === 'follows' ? 'follows' : 'others';
}

export function emptyDaemonMeshStatus(): DaemonMeshStatus {
  return {
    enabled: false,
    totalPeers: 0,
    connected: 0,
    withDataChannel: 0,
    transportCounts: {
      webrtc: 0,
      bluetooth: 0,
    },
    totalBytesSent: 0,
    totalBytesReceived: 0,
    peers: [],
    bluetoothReceivedEvents: [],
    blossomServers: 0,
  };
}

export function parseDaemonMeshStatus(payload: unknown): DaemonMeshStatus {
  const root = asRecord(payload);
  const mesh = asRecord(root?.mesh) ?? asRecord(root?.webrtc);
  const upstream = asRecord(root?.upstream);
  if (!mesh || mesh.enabled !== true) {
    return {
      ...emptyDaemonMeshStatus(),
      bluetoothReceivedEvents: readBluetoothReceivedEvents(mesh?.bluetooth_received_events),
      blossomServers: readNumber(upstream?.blossom_servers),
    };
  }

  const rawPeers = Array.isArray(mesh.peers) ? mesh.peers : [];
  const peers = rawPeers
    .map((entry) => asRecord(entry))
    .filter((entry): entry is Record<string, unknown> => !!entry)
    .map((entry) => {
      const peerId = readString(entry.peer_id) || readString(entry.id);
      const state: MeshPeerState =
        entry.connected === true || readString(entry.state).toLowerCase() === 'connected'
          ? 'connected'
          : 'disconnected';
      return {
        id: readString(entry.id, peerId),
        peerId,
        pubkey: readString(entry.pubkey),
        state,
        pool: normalizePool(readString(entry.pool)),
        bytesSent: readNumber(entry.bytes_sent),
        bytesReceived: readNumber(entry.bytes_received),
        transport: readString(entry.transport, 'webrtc'),
        signalPaths: readStringArray(entry.signal_paths),
      };
    });

  const transportCountsRecord = asRecord(mesh.transport_counts);
  return {
    enabled: true,
    totalPeers: readNumber(mesh.total_peers) || peers.length,
    connected: readNumber(mesh.connected) || peers.filter((peer) => peer.state === 'connected').length,
    withDataChannel: readNumber(mesh.with_data_channel),
    transportCounts: {
      webrtc: readNumber(transportCountsRecord?.webrtc),
      bluetooth: readNumber(transportCountsRecord?.bluetooth),
    },
    totalBytesSent: readNumber(mesh.bytes_sent) || calculateMeshTotals(peers).totalBytesSent,
    totalBytesReceived: readNumber(mesh.bytes_received) || calculateMeshTotals(peers).totalBytesReceived,
    peers,
    bluetoothReceivedEvents: readBluetoothReceivedEvents(mesh.bluetooth_received_events),
    blossomServers: readNumber(upstream?.blossom_servers),
  };
}

function readBluetoothReceivedEvents(value: unknown): BluetoothReceivedEventInfo[] {
  const entries = Array.isArray(value) ? value : [];
  return entries
    .map((entry) => asRecord(entry))
    .filter((entry): entry is Record<string, unknown> => !!entry)
    .map((entry) => ({
      eventId: readString(entry.event_id),
      pubkey: readString(entry.pubkey),
      kind: readNumber(entry.kind),
      createdAt: readNumber(entry.created_at),
      receivedAt: readNumber(entry.received_at),
      peerId: readString(entry.peer_id) || null,
      cidValues: readStringArray(entry.cid_values),
    }));
}

export function calculateMeshTotals(
  peers: readonly Pick<MeshPeerInfo, 'bytesSent' | 'bytesReceived'>[],
): MeshTotals {
  return peers.reduce<MeshTotals>(
    (totals, peer) => ({
      totalBytesSent: totals.totalBytesSent + readNumber(peer.bytesSent),
      totalBytesReceived: totals.totalBytesReceived + readNumber(peer.bytesReceived),
    }),
    {
      totalBytesSent: 0,
      totalBytesReceived: 0,
    },
  );
}

export function advanceMeshBandwidthHistory(
  previous: MeshHistoryCursor | null,
  history: readonly MeshBandwidthHistoryPoint[],
  totals: MeshTotals,
  timestamp: number,
  maxPoints = 60,
): {
  nextCursor: MeshHistoryCursor;
  rates: { uploadBps: number; downloadBps: number };
  history: MeshBandwidthHistoryPoint[];
} {
  const nextCursor: MeshHistoryCursor = {
    timestamp,
    totalBytesSent: totals.totalBytesSent,
    totalBytesReceived: totals.totalBytesReceived,
  };

  if (!previous || previous.timestamp <= 0 || timestamp <= previous.timestamp) {
    return {
      nextCursor,
      rates: { uploadBps: 0, downloadBps: 0 },
      history: Array.from(history)
        .slice(-Math.max(0, maxPoints - 1))
        .concat({
          timestamp,
          totalBytesSent: totals.totalBytesSent,
          totalBytesReceived: totals.totalBytesReceived,
          uploadBps: 0,
          downloadBps: 0,
        }),
    };
  }

  const elapsedSeconds = Math.max((timestamp - previous.timestamp) / 1000, 0.001);
  const uploadBytes = Math.max(0, totals.totalBytesSent - previous.totalBytesSent);
  const downloadBytes = Math.max(0, totals.totalBytesReceived - previous.totalBytesReceived);
  const point: MeshBandwidthHistoryPoint = {
    timestamp,
    totalBytesSent: totals.totalBytesSent,
    totalBytesReceived: totals.totalBytesReceived,
    uploadBps: uploadBytes / elapsedSeconds,
    downloadBps: downloadBytes / elapsedSeconds,
  };

  return {
    nextCursor,
    rates: { uploadBps: point.uploadBps, downloadBps: point.downloadBps },
    history: Array.from(history).slice(-Math.max(0, maxPoints - 1)).concat(point),
  };
}

export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
  return `${(bytes / 1024 / 1024 / 1024).toFixed(1)} GB`;
}

export function formatBandwidth(bytesPerSecond: number): string {
  if (bytesPerSecond < 1) return '0 B/s';
  if (bytesPerSecond < 1024) return `${Math.round(bytesPerSecond)} B/s`;
  if (bytesPerSecond < 1024 * 1024) return `${(bytesPerSecond / 1024).toFixed(1)} KB/s`;
  return `${(bytesPerSecond / 1024 / 1024).toFixed(1)} MB/s`;
}

export function shortIdentifier(value: string, head = 8, tail = 6): string {
  if (value.length <= head + tail + 1) return value;
  return `${value.slice(0, head)}...${value.slice(-tail)}`;
}
