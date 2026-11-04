package voice

import (
	"errors"
	"testing"

	"github.com/hashimthearab/rust-mcbe/core/internal/bridgecontract"
)

func TestParseVoiceIPCCommandRequiresExactFrameAndGateShapes(t *testing.T) {
	frame := make([]byte, bridgecontract.VoiceFrameBytes+1)
	frame[0] = bridgecontract.VoiceIPCKindAudio
	command, err := parseVoiceIPCCommand(frame)
	if err != nil || len(command.Frame) != bridgecontract.VoiceFrameBytes {
		t.Fatalf("valid audio command: command=%#v err=%v", command, err)
	}

	for _, payload := range [][]byte{
		nil,
		{bridgecontract.VoiceIPCKindAudio},
		append([]byte{bridgecontract.VoiceIPCKindAudio}, make([]byte, bridgecontract.VoiceFrameBytes-1)...),
		{bridgecontract.VoiceIPCKindTransmit},
		{bridgecontract.VoiceIPCKindTransmit, 2},
		{bridgecontract.VoiceIPCKindClose, 0},
		{255},
		make([]byte, bridgecontract.VoiceMaxFrameLen+1),
	} {
		if _, err := parseVoiceIPCCommand(payload); !errors.Is(err, ErrVoiceFrameInvalid) {
			t.Fatalf("payload length %d kind %d error = %v, want invalid", len(payload), firstByte(payload), err)
		}
	}
}

func TestParseVoiceIPCCommandParsesTransmitAndClose(t *testing.T) {
	enabled, err := parseVoiceIPCCommand([]byte{bridgecontract.VoiceIPCKindTransmit, 1})
	if err != nil || !enabled.Transmit {
		t.Fatalf("enabled command = %#v, err=%v", enabled, err)
	}
	disabled, err := parseVoiceIPCCommand([]byte{bridgecontract.VoiceIPCKindTransmit, 0})
	if err != nil || disabled.Transmit {
		t.Fatalf("disabled command = %#v, err=%v", disabled, err)
	}
	closeCommand, err := parseVoiceIPCCommand([]byte{bridgecontract.VoiceIPCKindClose})
	if err != nil || !closeCommand.Close {
		t.Fatalf("close command = %#v, err=%v", closeCommand, err)
	}
}

func firstByte(payload []byte) byte {
	if len(payload) == 0 {
		return 0
	}
	return payload[0]
}
