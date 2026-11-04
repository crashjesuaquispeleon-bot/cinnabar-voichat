package voice

import (
	"context"
	"encoding/binary"
	"testing"
	"time"

	"github.com/pion/webrtc/v4"
)

func TestP2PPeersExchangeAudioInBothDirectionsAndClosePTT(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 35*time.Second)
	defer cancel()

	host, err := NewP2PPeer(nil, true)
	if err != nil { t.Fatal(err) }
	defer host.Close()
	guest, err := NewP2PPeer(nil, false)
	if err != nil { t.Fatal(err) }
	defer guest.Close()

	offer, err := host.CreateOffer(ctx)
	if err != nil { t.Fatalf("create offer: %v", err) }
	answer, err := guest.AcceptOffer(ctx, offer)
	if err != nil { t.Fatalf("accept offer: %v", err) }
	if err := host.AcceptAnswer(answer); err != nil { t.Fatalf("accept answer: %v", err) }

	if err := host.WaitReady(ctx); err != nil { t.Fatalf("host data channel: %v", err) }
	if err := guest.WaitReady(ctx); err != nil { t.Fatalf("guest data channel: %v", err) }

	frame := make([]byte, voiceFrameBytes)
	binary.BigEndian.PutUint64(frame[:voiceSequenceBytes], 1)
	frame[voiceSequenceBytes] = 0x4a
	if err := host.SendFrame(frame); err != ErrVoiceTransmitDisabled {
		t.Fatalf("muted host sent audio: %v", err)
	}
	host.SetTransmitAllowed(true)
	if err := host.SendFrame(frame); err != nil { t.Fatalf("host send: %v", err) }
	select {
	case packet := <-guest.ReceiveFrames():
		if len(packet) != voiceRemoteFrameBytes || string(packet[voiceSourceIDBytes:]) != string(frame) {
			t.Fatal("guest received malformed host frame")
		}
		if string(packet[:voiceSourceIDBytes]) == string(make([]byte, voiceSourceIDBytes)) {
			t.Fatal("host source identifier is empty")
		}
	case <-ctx.Done(): t.Fatal("guest did not receive host audio")
	}

	guestFrame := make([]byte, voiceFrameBytes)
	binary.BigEndian.PutUint64(guestFrame[:voiceSequenceBytes], 1)
	guestFrame[voiceSequenceBytes] = 0x6b
	guest.SetTransmitAllowed(true)
	if err := guest.SendFrame(guestFrame); err != nil { t.Fatalf("guest send: %v", err) }
	select {
	case packet := <-host.ReceiveFrames():
		if len(packet) != voiceRemoteFrameBytes || string(packet[voiceSourceIDBytes:]) != string(guestFrame) {
			t.Fatal("host received malformed guest frame")
		}
	case <-ctx.Done(): t.Fatal("host did not receive guest audio")
	}

	host.SetTransmitAllowed(false)
	second := make([]byte, voiceFrameBytes)
	binary.BigEndian.PutUint64(second[:voiceSequenceBytes], 2)
	if err := host.SendFrame(second); err != ErrVoiceTransmitDisabled {
		t.Fatalf("host transmitted after PTT release: %v", err)
	}
	host.Close()
	select {
	case <-host.Done():
	case <-time.After(time.Second): t.Fatal("host close did not signal Done")
	}
	if err := host.SendFrame(frame); err != ErrVoiceTransmitDisabled {
		t.Fatalf("closed host accepted audio: %v", err)
	}
}

func TestP2PPeerRejectsWrongNegotiationRole(t *testing.T) {
	peer, err := NewP2PPeer(nil, false)
	if err != nil { t.Fatal(err) }
	defer peer.Close()
	if _, err := peer.AcceptOffer(context.Background(), webrtc.SessionDescription{Type: webrtc.SDPTypeAnswer, SDP: "invalid"}); err != ErrP2PPeerRole {
		t.Fatalf("answer accepted as offer: %v", err)
	}
	if err := peer.AcceptAnswer(webrtc.SessionDescription{Type: webrtc.SDPTypeOffer, SDP: "invalid"}); err != ErrP2PPeerRole {
		t.Fatalf("offer accepted as answer: %v", err)
	}
}
