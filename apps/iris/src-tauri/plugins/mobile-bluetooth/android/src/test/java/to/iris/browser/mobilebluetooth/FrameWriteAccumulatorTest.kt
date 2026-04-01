package to.iris.browser.mobilebluetooth

import java.nio.ByteBuffer
import java.nio.charset.StandardCharsets
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

class FrameWriteAccumulatorTest {
    @Test
    fun reassemblesFrameFromOutOfOrderOffsetChunks() {
        val accumulator = FrameWriteAccumulator()
        val payload = "bluetooth publish sync ".repeat(12).toByteArray(StandardCharsets.UTF_8)
        val frame = encodeTestFrame("text", payload)

        val secondChunk = frame.copyOfRange(48, frame.size)
        val firstChunk = frame.copyOfRange(0, 48)

        assertTrue(accumulator.append("peer-a", 48, secondChunk).isEmpty())

        val decoded = accumulator.append("peer-a", 0, firstChunk)
        assertEquals(1, decoded.size)
        assertEquals("text", decoded.single().kind)
        assertEquals(String(payload, StandardCharsets.UTF_8), decoded.single().payload.toString(StandardCharsets.UTF_8))
    }

    @Test
    fun resetsStateAfterDecodingCompletedFrame() {
        val accumulator = FrameWriteAccumulator()
        val first = encodeTestFrame("text", "first frame".toByteArray(StandardCharsets.UTF_8))
        val second = encodeTestFrame("binary", byteArrayOf(1, 2, 3, 4, 5))

        assertEquals(
            "first frame",
            accumulator
                .append("peer-a", 0, first)
                .single()
                .payload
                .toString(StandardCharsets.UTF_8)
        )

        val secondDecoded = accumulator.append("peer-a", 0, second)
        assertEquals(1, secondDecoded.size)
        assertEquals("binary", secondDecoded.single().kind)
        assertTrue(secondDecoded.single().payload.contentEquals(byteArrayOf(1, 2, 3, 4, 5)))
    }

    @Test
    fun treatsRepeatedZeroOffsetsAsSequentialChunksForOneFrame() {
        val accumulator = FrameWriteAccumulator()
        val payload = "streamed hello payload ".repeat(8).toByteArray(StandardCharsets.UTF_8)
        val frame = encodeTestFrame("text", payload)

        assertTrue(accumulator.append("peer-a", 0, frame.copyOfRange(0, 64)).isEmpty())

        val decoded = accumulator.append("peer-a", 0, frame.copyOfRange(64, frame.size))
        assertEquals(1, decoded.size)
        assertEquals(String(payload, StandardCharsets.UTF_8), decoded.single().payload.toString(StandardCharsets.UTF_8))
    }

    private fun encodeTestFrame(kind: String, payload: ByteArray): ByteArray {
        val header = ByteBuffer.allocate(5)
        header.put(if (kind == "text") 1 else 2)
        header.putInt(payload.size)
        return header.array() + payload
    }
}
