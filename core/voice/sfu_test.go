package voice

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/binary"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/google/uuid"
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

// TestSFURelaysOnlyBetweenPeersWithTheSameSignedRoom proves that the native Go client,
// the WebSocket signaling endpoint and the room relay work together with local Pion peers.
func TestSFURelaysOnlyBetweenPeersWithTheSameSignedRoom(t *testing.T) {
	now := time.Now()
	private, verify := verifier(t)
	roomID, sessionID := uuid.NewString(), uuid.NewString()
	firstClaims := claimsAt(now)
	firstClaims.RoomID, firstClaims.SessionID, firstClaims.XUID = roomID, sessionID, "10001"
	secondClaims := claimsAt(now)
	secondClaims.RoomID, secondClaims.SessionID, secondClaims.XUID = roomID, sessionID, "10002"
	isolatedClaims := claimsAt(now)
	isolatedClaims.XUID = "10003"

	sfu, err := NewSFU(SFUConfig{Admission: verify, MaxParticipants: 4})
	if err != nil {
		t.Fatal(err)
	}
	server := httptest.NewTLSServer(http.HandlerFunc(sfu.ServeHTTP))
	defer server.Close()
	defer sfu.Close()
	endpoint := "wss" + strings.TrimPrefix(server.URL, "https")
	httpClient := &http.Client{Transport: &http.Transport{
		TLSClientConfig: &tls.Config{InsecureSkipVerify: true}, // test server uses a self-signed certificate
	}}
	ctx, cancel := context.WithTimeout(context.Background(), 35*time.Second)
	defer cancel()

	first, err := DialClient(ctx, ClientConfig{
		Endpoint: endpoint, AdmissionToken: signedToken(t, private, firstClaims), HTTPClient: httpClient,
	})
	if err != nil {
		t.Fatalf("connect first voice client: %v", err)
	}
	defer first.Close()
	second, err := DialClient(ctx, ClientConfig{
		Endpoint: endpoint, AdmissionToken: signedToken(t, private, secondClaims), HTTPClient: httpClient,
	})
	if err != nil {
		t.Fatalf("connect second voice client: %v", err)
	}
	defer second.Close()
	isolated, err := DialClient(ctx, ClientConfig{
		Endpoint: endpoint, AdmissionToken: signedToken(t, private, isolatedClaims), HTTPClient: httpClient,
	})
	if err != nil {
		t.Fatalf("connect isolated voice client: %v", err)
	}
	defer isolated.Close()

	frame := make([]byte, voiceFrameBytes)
	binary.BigEndian.PutUint64(frame[:voiceSequenceBytes], 1)
	for i := voiceSequenceBytes; i < len(frame); i++ {
		frame[i] = byte(i)
	}
	if err := first.SendFrame(frame); err != ErrVoiceTransmitDisabled {
		t.Fatalf("frame sent before PTT gate opened: %v", err)
	}
	if err := first.SetTransmitAllowed(ctx, true); err != nil {
		t.Fatalf("open server-authorized transmit gate: %v", err)
	}
	if err := first.SendFrame(frame); err != nil {
		t.Fatalf("send first peer frame: %v", err)
	}
	select {
	case received := <-second.ReceiveFrames():
		if !bytes.Equal(received, frame) {
			t.Fatal("second peer received a different voice frame")
		}
	case <-ctx.Done():
		t.Fatal("second peer did not receive the first peer's voice frame")
	}

	if err := first.SetTransmitAllowed(ctx, false); err != nil {
		t.Fatalf("close server-authorized transmit gate: %v", err)
	}
	if err := first.SendFrame(frame); err != ErrVoiceTransmitDisabled {
		t.Fatalf("frame sent after PTT gate closed: %v", err)
	}
	select {
	case <-first.ReceiveFrames():
		t.Fatal("SFU looped the sender's voice frame back to its origin")
	case <-time.After(150 * time.Millisecond):
	}
	select {
	case leaked := <-isolated.ReceiveFrames():
		t.Fatalf("SFU leaked a room frame into another session: %d bytes", len(leaked))
	case <-time.After(150 * time.Millisecond):
	}
}

func TestVoiceFrameRateLimiterAllowsShortBurstAndCapsSustainedRate(t *testing.T) {
	peer := &sfuPeer{}
	now := time.Unix(1_800_000_000, 0)
	for i := 0; i < 10; i++ {
		if !peer.allowVoiceFrame(now) {
			t.Fatalf("frame %d unexpectedly rejected within burst", i+1)
		}
	}
	if peer.allowVoiceFrame(now) {
		t.Fatal("frame above burst allowance accepted")
	}
	if !peer.allowVoiceFrame(now.Add(20 * time.Millisecond)) {
		t.Fatal("one replenished frame was not admitted after 20 ms")
	}
}
