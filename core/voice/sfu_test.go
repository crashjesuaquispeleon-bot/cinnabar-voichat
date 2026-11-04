package voice

import (
	"encoding/binary"
	"testing"
)

func TestVoiceFrameSequenceRequiresExactPcmFrameAndNewSequence(t *testing.T) {
	frame := make([]byte, voiceFrameBytes)
	binary.BigEndian.PutUint64(frame[:voiceSequenceBytes], 7)
	if sequence, ok := voiceFrameSequence(frame, 6); !ok || sequence != 7 {
		t.Fatalf("valid sequence = %d, %t", sequence, ok)
	}
	if _, ok := voiceFrameSequence(frame, 7); ok {
		t.Fatal("duplicate sequence accepted")
	}
	if _, ok := voiceFrameSequence(frame, 8); ok {
		t.Fatal("out-of-order sequence accepted")
	}
	if _, ok := voiceFrameSequence(frame[:len(frame)-1], 0); ok {
		t.Fatal("short PCM frame accepted")
	}
	if _, ok := voiceFrameSequence(append(frame, 0), 0); ok {
		t.Fatal("oversized PCM frame accepted")
	}
}

func TestVoiceFrameFormatIsTwentyMillisecondsOfMonoPcm16(t *testing.T) {
	if voiceSamplesPerFrame != 320 || voicePCMBytesPerFrame != 640 || voiceFrameBytes != 648 {
		t.Fatalf("unexpected voice frame format: samples=%d pcm=%d payload=%d",
			voiceSamplesPerFrame, voicePCMBytesPerFrame, voiceFrameBytes)
	}
}
