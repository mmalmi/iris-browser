import CoreBluetooth
import Foundation
import Tauri

struct StartArgs: Decodable {
  let localPeerId: String
}

struct SendArgs: Decodable {
  let address: String
  let kind: String
  let payloadBase64: String
}

private struct AddressEvent: Encodable {
  let address: String
}

private struct FrameEvent: Encodable {
  let address: String
  let kind: String
  let payloadBase64: String
}

private struct PeerSnapshot: Encodable {
  let address: String
  let ready: Bool
}

private struct TransportPollResponse: Encodable {
  let peers: [PeerSnapshot]
  let frames: [FrameEvent]
}

private struct DecodedFrame {
  let kind: String
  let payload: Data
}

private struct PendingSend {
  let address: String
  let chunks: [Data]
  var nextIndex: Int
  let invoke: Invoke?
}

private let serviceUUID = CBUUID(string: "f18ef5f6-b7ee-4f40-b869-10a2d4f35932")
private let rxUUID = CBUUID(string: "0bb5f5c9-6369-4511-a84f-4d4c14d8f8d4")
private let txUUID = CBUUID(string: "4ec9c0c2-97c6-4f46-9fd1-927d699b2f6d")
private let userDescriptionUUID = CBUUID(string: "2901")
private let chunkBytes = 64

private final class FrameDecoder {
  private var buffer = [UInt8]()

  func append(_ chunk: Data) -> [DecodedFrame] {
    buffer.append(contentsOf: chunk)
    var frames = [DecodedFrame]()

    while buffer.count >= 5 {
      let length =
        (Int(buffer[1]) << 24) | (Int(buffer[2]) << 16) | (Int(buffer[3]) << 8) | Int(buffer[4])
      guard buffer.count >= 5 + length else {
        break
      }

      let payload = Data(buffer[5..<(5 + length)])
      let kind: String
      switch buffer[0] {
      case 1:
        kind = "text"
      case 2:
        kind = "binary"
      default:
        buffer.removeAll()
        return frames
      }

      frames.append(DecodedFrame(kind: kind, payload: payload))
      buffer.removeFirst(5 + length)
    }

    return frames
  }
}

private final class FrameWriteAccumulator {
  private struct PendingWrite {
    let buffer: Data
    let receivedRanges: [ClosedRange<Int>]
  }

  private var pendingWrites = [String: PendingWrite]()

  func append(address: String, offset: Int, chunk: Data) -> [DecodedFrame] {
    guard !chunk.isEmpty else {
      return []
    }

    let existing = pendingWrites[address]
    let effectiveOffset = normalizedOffset(existing: existing, requestedOffset: offset)
    let end = effectiveOffset + chunk.count
    var combined = existing?.buffer ?? Data()
    if combined.count < end {
      combined.append(Data(repeating: 0, count: end - combined.count))
    }
    combined.replaceSubrange(effectiveOffset..<end, with: chunk)

    var pending = PendingWrite(
      buffer: combined,
      receivedRanges: mergeRanges(existing?.receivedRanges ?? [], next: effectiveOffset...(end - 1))
    )

    var decoded = [DecodedFrame]()
    while true {
      guard let contiguousBytes = contiguousPrefixLength(pending.receivedRanges),
        let frameLength = encodedFrameLength(buffer: pending.buffer, contiguousBytes: contiguousBytes),
        contiguousBytes >= frameLength
      else {
        break
      }

      let frames = FrameDecoder().append(pending.buffer.prefix(frameLength))
      guard frames.count == 1 else {
        pendingWrites.removeValue(forKey: address)
        return decoded
      }
      decoded.append(contentsOf: frames)

      pending = trimPendingWrite(pending, consumedBytes: frameLength)
      if pending.buffer.isEmpty {
        pendingWrites.removeValue(forKey: address)
        return decoded
      }
    }

    pendingWrites[address] = pending
    return decoded
  }

  func clear(address: String) {
    pendingWrites.removeValue(forKey: address)
  }

  func clearAll() {
    pendingWrites.removeAll()
  }

  private func contiguousPrefixLength(_ ranges: [ClosedRange<Int>]) -> Int? {
    guard let first = ranges.first, first.lowerBound == 0 else {
      return nil
    }

    var prefixEnd = first.upperBound
    for range in ranges.dropFirst() {
      if range.lowerBound > prefixEnd + 1 {
        break
      }
      prefixEnd = max(prefixEnd, range.upperBound)
    }
    return prefixEnd + 1
  }

  private func normalizedOffset(existing: PendingWrite?, requestedOffset: Int) -> Int {
    guard requestedOffset == 0 else {
      return requestedOffset
    }
    guard let existing, let contiguousBytes = contiguousPrefixLength(existing.receivedRanges) else {
      return 0
    }
    return contiguousBytes == existing.buffer.count ? existing.buffer.count : 0
  }

  private func encodedFrameLength(buffer: Data, contiguousBytes: Int) -> Int? {
    guard contiguousBytes >= 5 else {
      return nil
    }
    let header = [UInt8](buffer.dropFirst().prefix(4))
    guard header.count == 4 else {
      return nil
    }
    let payloadLength =
      (Int(header[0]) << 24) | (Int(header[1]) << 16) | (Int(header[2]) << 8) | Int(header[3])
    return payloadLength >= 0 ? 5 + payloadLength : nil
  }

  private func mergeRanges(_ existing: [ClosedRange<Int>], next: ClosedRange<Int>) -> [ClosedRange<Int>] {
    let sorted = (existing + [next]).sorted { $0.lowerBound < $1.lowerBound }
    guard var current = sorted.first else {
      return []
    }

    var merged = [ClosedRange<Int>]()
    for candidate in sorted.dropFirst() {
      if candidate.lowerBound <= current.upperBound + 1 {
        current = current.lowerBound...max(current.upperBound, candidate.upperBound)
      } else {
        merged.append(current)
        current = candidate
      }
    }
    merged.append(current)
    return merged
  }

  private func trimPendingWrite(_ pending: PendingWrite, consumedBytes: Int) -> PendingWrite {
    guard consumedBytes < pending.buffer.count else {
      return PendingWrite(buffer: Data(), receivedRanges: [])
    }

    let trimmedBuffer = pending.buffer.dropFirst(consumedBytes)
    let shiftedRanges = pending.receivedRanges.compactMap { range -> ClosedRange<Int>? in
      let shiftedLower = range.lowerBound - consumedBytes
      let shiftedUpper = range.upperBound - consumedBytes
      guard shiftedUpper >= 0 else {
        return nil
      }
      return max(0, shiftedLower)...shiftedUpper
    }

    let mergedShiftedRanges = shiftedRanges.reduce(into: [ClosedRange<Int>]()) { result, range in
      result = mergeRanges(result, next: range)
    }
    return PendingWrite(buffer: Data(trimmedBuffer), receivedRanges: mergedShiftedRanges)
  }
}

private func encodeFrame(kind: String, payload: Data) -> Data? {
  let kindByte: UInt8
  switch kind {
  case "text":
    kindByte = 1
  case "binary":
    kindByte = 2
  default:
    return nil
  }

  let length = UInt32(payload.count).bigEndian
  var header = Data([kindByte])
  withUnsafeBytes(of: length) { bytes in
    header.append(contentsOf: bytes)
  }
  var frame = header
  frame.append(payload)
  return frame
}

private func helloPayload(localPeerId: String) -> Data {
  Data(#"{"type":"hello","peerId":"\#(localPeerId)"}"#.utf8)
}

private extension Data {
  func chunked(maxLength: Int) -> [Data] {
    guard !isEmpty else {
      return [Data()]
    }

    var chunks = [Data]()
    var index = startIndex
    while index < endIndex {
      let nextIndex = self.index(index, offsetBy: maxLength, limitedBy: endIndex) ?? endIndex
      chunks.append(self[index..<nextIndex])
      index = nextIndex
    }
    return chunks
  }
}

class MobileBluetoothPlugin: Plugin, CBPeripheralManagerDelegate {
  private var peripheralManager: CBPeripheralManager?
  private var rxCharacteristic: CBMutableCharacteristic?
  private var txCharacteristic: CBMutableCharacteristic?
  private var localPeerId = ""
  private var desiredActive = false
  private var bluetoothActive = false
  private var serviceRegistrationPending = false
  private var advertisingPending = false
  private var peers = [String: CBCentral]()
  private var readyPeers = Set<String>()
  private var writeAccumulator = FrameWriteAccumulator()
  private var drainedFrames = [FrameEvent]()
  private var pendingSends = [PendingSend]()
  private var pendingStartInvoke: Invoke?

  @objc public func start(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(StartArgs.self)

    DispatchQueue.main.async {
      self.pendingStartInvoke?.reject("Bluetooth start superseded by a newer request")
      self.pendingStartInvoke = invoke
      self.localPeerId = args.localPeerId
      self.desiredActive = true
      self.resetTransportState(rejectPendingSends: true)
      self.ensurePeripheralManager()
      self.maybeStartPeripheral()
    }
  }

  @objc public func stop(_ invoke: Invoke) throws {
    DispatchQueue.main.async {
      self.desiredActive = false
      self.failPendingStart("Bluetooth stopped")
      self.resetTransportState(rejectPendingSends: true)
      invoke.resolve()
    }
  }

  @objc public func send(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(SendArgs.self)

    DispatchQueue.main.async {
      guard self.bluetoothActive else {
        invoke.reject("Bluetooth peer not connected")
        return
      }
      guard let payload = Data(base64Encoded: args.payloadBase64) else {
        invoke.reject("Invalid Bluetooth payload")
        return
      }
      self.queueFrame(to: args.address, kind: args.kind, payload: payload, invoke: invoke)
    }
  }

  @objc public func pollTransport(_ invoke: Invoke) throws {
    DispatchQueue.main.async {
      let peers = self.peers.keys.sorted().map { address in
        PeerSnapshot(address: address, ready: self.readyPeers.contains(address))
      }
      let frames = self.drainedFrames
      self.drainedFrames.removeAll(keepingCapacity: true)
      invoke.resolve(TransportPollResponse(peers: peers, frames: frames))
    }
  }

  func peripheralManagerDidUpdateState(_ peripheral: CBPeripheralManager) {
    DispatchQueue.main.async {
      switch peripheral.state {
      case .poweredOn:
        self.maybeStartPeripheral()
      case .poweredOff:
        self.failActiveSession("Bluetooth is disabled")
      case .unauthorized:
        self.failActiveSession("Bluetooth permission denied")
      case .unsupported:
        self.failActiveSession("Bluetooth LE peripheral is unavailable")
      case .resetting, .unknown:
        self.bluetoothActive = false
      @unknown default:
        self.failActiveSession("Bluetooth state is unavailable")
      }
    }
  }

  func peripheralManager(_ peripheral: CBPeripheralManager, didAdd service: CBService, error: Error?) {
    DispatchQueue.main.async {
      guard service.uuid == serviceUUID else {
        return
      }
      self.serviceRegistrationPending = false
      if let error = error {
        self.failActiveSession("Failed to add Bluetooth GATT service: \(error.localizedDescription)")
        return
      }

      self.advertisingPending = true
      peripheral.startAdvertising([
        CBAdvertisementDataServiceUUIDsKey: [serviceUUID]
      ])
    }
  }

  func peripheralManagerDidStartAdvertising(_ peripheral: CBPeripheralManager, error: Error?) {
    DispatchQueue.main.async {
      self.advertisingPending = false
      if let error = error {
        self.failActiveSession("Failed to start Bluetooth advertising: \(error.localizedDescription)")
        return
      }

      self.bluetoothActive = true
      self.pendingStartInvoke?.resolve()
      self.pendingStartInvoke = nil
    }
  }

  func peripheralManager(_ peripheral: CBPeripheralManager, didReceiveRead request: CBATTRequest) {
    DispatchQueue.main.async {
      guard request.characteristic.uuid == txUUID else {
        peripheral.respond(to: request, withResult: .requestNotSupported)
        return
      }

      _ = self.rememberPeer(request.central)
      guard let frame = encodeFrame(kind: "text", payload: helloPayload(localPeerId: self.localPeerId)) else {
        peripheral.respond(to: request, withResult: .unlikelyError)
        return
      }
      guard request.offset <= frame.count else {
        peripheral.respond(to: request, withResult: .invalidOffset)
        return
      }

      request.value = frame.subdata(in: request.offset..<frame.count)
      peripheral.respond(to: request, withResult: .success)
    }
  }

  func peripheralManager(_ peripheral: CBPeripheralManager, didReceiveWrite requests: [CBATTRequest]) {
    DispatchQueue.main.async {
      guard let firstRequest = requests.first else {
        return
      }

      var result: CBATTError.Code = .success
      for request in requests {
        guard request.characteristic.uuid == rxUUID else {
          result = .requestNotSupported
          break
        }

        let address = self.rememberPeer(request.central)
        guard let value = request.value else {
          continue
        }

        for frame in self.writeAccumulator.append(address: address, offset: request.offset, chunk: value) {
          self.triggerFrame(address: address, kind: frame.kind, payload: frame.payload)
        }
      }

      peripheral.respond(to: firstRequest, withResult: result)
    }
  }

  func peripheralManager(
    _ peripheral: CBPeripheralManager,
    central: CBCentral,
    didSubscribeTo characteristic: CBCharacteristic
  ) {
    DispatchQueue.main.async {
      guard characteristic.uuid == txUUID else {
        return
      }

      let address = self.rememberPeer(central)
      self.readyPeers.insert(address)
      self.sendHello(to: address)
      self.triggerAddress("peer-ready", address: address)
    }
  }

  func peripheralManager(
    _ peripheral: CBPeripheralManager,
    central: CBCentral,
    didUnsubscribeFrom characteristic: CBCharacteristic
  ) {
    DispatchQueue.main.async {
      guard characteristic.uuid == txUUID else {
        return
      }

      self.removePeer(address: self.address(for: central), notify: true)
    }
  }

  func peripheralManagerIsReady(toUpdateSubscribers peripheral: CBPeripheralManager) {
    DispatchQueue.main.async {
      self.flushPendingSends()
    }
  }

  private func ensurePeripheralManager() {
    if peripheralManager == nil {
      peripheralManager = CBPeripheralManager(
        delegate: self,
        queue: nil,
        options: [CBPeripheralManagerOptionShowPowerAlertKey: true]
      )
    } else {
      peripheralManager?.delegate = self
    }
  }

  private func maybeStartPeripheral() {
    guard desiredActive, let peripheral = peripheralManager else {
      return
    }

    switch peripheral.state {
    case .poweredOn:
      guard !bluetoothActive, !serviceRegistrationPending, !advertisingPending else {
        return
      }

      let rx = CBMutableCharacteristic(
        type: rxUUID,
        properties: [.write, .writeWithoutResponse],
        value: nil,
        permissions: [.writeable]
      )
      // macOS btleplug waits for descriptor discovery on every characteristic before
      // considering the connection fully established.
      rx.descriptors = [
        CBMutableDescriptor(type: userDescriptionUUID, value: "iris-rx" as NSString)
      ]
      let tx = CBMutableCharacteristic(
        type: txUUID,
        properties: [.notify, .read],
        value: nil,
        permissions: [.readable]
      )

      let service = CBMutableService(type: serviceUUID, primary: true)
      service.characteristics = [rx, tx]

      rxCharacteristic = rx
      txCharacteristic = tx
      serviceRegistrationPending = true
      peripheral.add(service)
    case .poweredOff:
      failActiveSession("Bluetooth is disabled")
    case .unauthorized:
      failActiveSession("Bluetooth permission denied")
    case .unsupported:
      failActiveSession("Bluetooth LE peripheral is unavailable")
    case .resetting, .unknown:
      break
    @unknown default:
      failActiveSession("Bluetooth state is unavailable")
    }
  }

  private func failActiveSession(_ message: String) {
    let wasActive = desiredActive || bluetoothActive || serviceRegistrationPending || advertisingPending
    desiredActive = false
    failPendingStart(message)
    if wasActive {
      resetTransportState(rejectPendingSends: true)
    }
  }

  private func failPendingStart(_ message: String) {
    pendingStartInvoke?.reject(message)
    pendingStartInvoke = nil
  }

  private func resetTransportState(rejectPendingSends: Bool) {
    peripheralManager?.stopAdvertising()
    peripheralManager?.removeAllServices()
    bluetoothActive = false
    serviceRegistrationPending = false
    advertisingPending = false
    rxCharacteristic = nil
    txCharacteristic = nil
    peers.removeAll()
    readyPeers.removeAll()
    writeAccumulator.clearAll()
    drainedFrames.removeAll(keepingCapacity: false)

    if rejectPendingSends {
      rejectAllPendingSends("Bluetooth stopped")
    } else {
      pendingSends.removeAll()
    }
  }

  private func queueFrame(to address: String, kind: String, payload: Data, invoke: Invoke?) {
    guard readyPeers.contains(address), peers[address] != nil else {
      invoke?.reject("Bluetooth peer is not ready for notifications")
      return
    }
    guard let frame = encodeFrame(kind: kind, payload: payload) else {
      invoke?.reject("Invalid Bluetooth frame kind")
      return
    }

    pendingSends.append(
      PendingSend(address: address, chunks: frame.chunked(maxLength: chunkBytes), nextIndex: 0, invoke: invoke)
    )
    flushPendingSends()
  }

  private func flushPendingSends() {
    guard let peripheral = peripheralManager, let txCharacteristic else {
      return
    }

    while !pendingSends.isEmpty {
      var send = pendingSends.removeFirst()
      guard readyPeers.contains(send.address), let central = peers[send.address] else {
        send.invoke?.reject("Bluetooth peer not connected")
        continue
      }

      while send.nextIndex < send.chunks.count {
        let chunk = send.chunks[send.nextIndex]
        if peripheral.updateValue(chunk, for: txCharacteristic, onSubscribedCentrals: [central]) {
          send.nextIndex += 1
        } else {
          pendingSends.insert(send, at: 0)
          return
        }
      }

      send.invoke?.resolve()
    }
  }

  private func sendHello(to address: String) {
    queueFrame(to: address, kind: "text", payload: helloPayload(localPeerId: localPeerId), invoke: nil)
  }

  private func rememberPeer(_ central: CBCentral) -> String {
    let address = self.address(for: central)
    if peers[address] == nil {
      peers[address] = central
      triggerAddress("peer-connected", address: address)
    } else {
      peers[address] = central
    }
    return address
  }

  private func removePeer(address: String, notify: Bool) {
    peers.removeValue(forKey: address)
    readyPeers.remove(address)
    writeAccumulator.clear(address: address)
    rejectPendingSends(for: address, message: "Bluetooth peer disconnected")
    if notify {
      triggerAddress("peer-disconnected", address: address)
    }
  }

  private func rejectPendingSends(for address: String, message: String) {
    var retained = [PendingSend]()
    for send in pendingSends {
      if send.address == address {
        send.invoke?.reject(message)
      } else {
        retained.append(send)
      }
    }
    pendingSends = retained
  }

  private func rejectAllPendingSends(_ message: String) {
    for send in pendingSends {
      send.invoke?.reject(message)
    }
    pendingSends.removeAll()
  }

  private func address(for central: CBCentral) -> String {
    central.identifier.uuidString
  }

  private func triggerAddress(_ event: String, address: String) {
    try? trigger(event, data: AddressEvent(address: address))
  }

  private func triggerFrame(address: String, kind: String, payload: Data) {
    drainedFrames.append(
      FrameEvent(address: address, kind: kind, payloadBase64: payload.base64EncodedString())
    )
  }
}

@_cdecl("init_plugin_mobile_bluetooth")
func initPlugin() -> Plugin {
  MobileBluetoothPlugin()
}
