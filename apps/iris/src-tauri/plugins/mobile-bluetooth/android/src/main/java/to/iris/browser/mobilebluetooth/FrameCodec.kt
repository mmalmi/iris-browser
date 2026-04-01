package to.iris.browser.mobilebluetooth

import java.nio.ByteBuffer
import java.util.concurrent.ConcurrentHashMap

internal data class DecodedFrame(val kind: String, val payload: ByteArray)

internal class FrameDecoder {
    private val buffer = ArrayList<Byte>()

    fun append(chunk: ByteArray): List<DecodedFrame> {
        val frames = mutableListOf<DecodedFrame>()
        chunk.forEach { buffer.add(it) }
        while (buffer.size >= 5) {
            val header = ByteBuffer.wrap(byteArrayOf(buffer[1], buffer[2], buffer[3], buffer[4]))
            val len = header.int
            if (buffer.size < 5 + len) {
                break
            }
            val kind = buffer[0].toInt()
            val payload = ByteArray(len)
            for (i in 0 until len) {
                payload[i] = buffer[5 + i]
            }
            repeat(5 + len) { buffer.removeAt(0) }
            frames.add(
                DecodedFrame(
                    if (kind == 1) "text" else "binary",
                    payload,
                )
            )
        }
        return frames
    }
}

internal class FrameWriteAccumulator {
    private data class PendingWrite(val buffer: ByteArray, val receivedRanges: List<IntRange>)

    private val pendingWrites = ConcurrentHashMap<String, PendingWrite>()

    fun append(address: String, offset: Int, chunk: ByteArray): List<DecodedFrame> {
        if (chunk.isEmpty()) {
            return emptyList()
        }

        val existing = pendingWrites[address]
        val effectiveOffset = normalizedOffset(existing, offset)
        val end = effectiveOffset + chunk.size
        val existingBuffer = existing?.buffer ?: ByteArray(0)
        val combined = if (existingBuffer.size >= end) {
            existingBuffer.copyOf()
        } else {
            existingBuffer.copyOf(end)
        }
        chunk.copyInto(combined, destinationOffset = effectiveOffset)
        var pending = PendingWrite(
            combined,
            mergeRanges(existing?.receivedRanges.orEmpty(), effectiveOffset..(end - 1)),
        )

        val decoded = mutableListOf<DecodedFrame>()
        while (true) {
            val contiguousBytes = contiguousPrefixLength(pending.receivedRanges) ?: break
            val frameLength = encodedFrameLength(pending.buffer, contiguousBytes) ?: break
            if (contiguousBytes < frameLength) {
                break
            }

            val frames = FrameDecoder().append(pending.buffer.copyOfRange(0, frameLength))
            if (frames.size != 1) {
                pendingWrites.remove(address)
                return decoded
            }
            decoded.addAll(frames)

            pending = trimPendingWrite(pending, frameLength)
            if (pending.buffer.isEmpty()) {
                pendingWrites.remove(address)
                return decoded
            }
        }

        pendingWrites[address] = pending
        return decoded
    }

    fun clear(address: String) {
        pendingWrites.remove(address)
    }

    fun clearAll() {
        pendingWrites.clear()
    }

    private fun contiguousPrefixLength(ranges: List<IntRange>): Int? {
        if (ranges.isEmpty() || ranges.first().first != 0) {
            return null
        }

        var prefixEnd = ranges.first().last
        for (range in ranges.drop(1)) {
            if (range.first > prefixEnd + 1) {
                break
            }
            prefixEnd = maxOf(prefixEnd, range.last)
        }
        return prefixEnd + 1
    }

    private fun normalizedOffset(existing: PendingWrite?, requestedOffset: Int): Int {
        if (requestedOffset != 0) {
            return requestedOffset
        }
        val pendingWrite = existing ?: return 0
        val contiguousBytes = contiguousPrefixLength(pendingWrite.receivedRanges) ?: return 0
        return if (contiguousBytes == pendingWrite.buffer.size) {
            pendingWrite.buffer.size
        } else {
            0
        }
    }

    private fun encodedFrameLength(buffer: ByteArray, contiguousBytes: Int): Int? {
        if (contiguousBytes < 5) {
            return null
        }
        val payloadLength = ByteBuffer.wrap(buffer, 1, 4).int
        return if (payloadLength >= 0) 5 + payloadLength else null
    }

    private fun mergeRanges(existing: List<IntRange>, next: IntRange): List<IntRange> {
        val sorted = buildList {
            addAll(existing)
            add(next)
        }.sortedBy { it.first }

        if (sorted.isEmpty()) {
            return emptyList()
        }

        val merged = mutableListOf<IntRange>()
        var current = sorted.first()
        for (candidate in sorted.drop(1)) {
            current = if (candidate.first <= current.last + 1) {
                current.first..maxOf(current.last, candidate.last)
            } else {
                merged.add(current)
                candidate
            }
        }
        merged.add(current)
        return merged
    }

    private fun trimPendingWrite(pending: PendingWrite, consumedBytes: Int): PendingWrite {
        if (consumedBytes >= pending.buffer.size) {
            return PendingWrite(ByteArray(0), emptyList())
        }

        val trimmedBuffer = pending.buffer.copyOfRange(consumedBytes, pending.buffer.size)
        val shiftedRanges = pending.receivedRanges.mapNotNull { range ->
            val shiftedStart = range.first - consumedBytes
            val shiftedEnd = range.last - consumedBytes
            if (shiftedEnd < 0) {
                null
            } else {
                maxOf(0, shiftedStart)..shiftedEnd
            }
        }

        val mergedShiftedRanges = shiftedRanges.fold(emptyList<IntRange>()) { acc, range ->
            mergeRanges(acc, range)
        }
        return PendingWrite(trimmedBuffer, mergedShiftedRanges)
    }
}
