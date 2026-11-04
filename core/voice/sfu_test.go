package voice

import (
	"bytes"
	"context"
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/google/uuid"
	"github.com/pion/webrtc/v4"
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


// TestSFURelaysOnlyBetweenPeersWithTheSameSignedRoom proves the Go signaling and
// data-channel path end to end with two local Pion clients. It does not require public ICE.
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

	first := connectTestVoicePeer(t, endpoint, signedToken(t, private, firstClaims))
	defer first.close()
	second := connectTestVoicePeer(t, endpoint, signedToken(t, private, secondClaims))
	defer second.close()
	isolated := connectTestVoicePeer(t, endpoint, signedToken(t, private, isolatedClaims))
	defer isolated.close()

	select {
	case <-first.downlinkOpen:
	case <-time.After(10 * time.Second):
		t.Fatal("first peer downlink did not open")
	}
	select {
	case <-second.downlinkOpen:
	case <-time.After(10 * time.Second):
		t.Fatal("second peer downlink did not open")
	}
	select {
	case <-isolated.downlinkOpen:
	case <-time.After(10 * time.Second):
		t.Fatal("isolated peer downlink did not open")
	}

	frame := make([]byte, voiceFrameBytes)
	binary.BigEndian.PutUint64(frame[:voiceSequenceBytes], 1)
	for i := voiceSequenceBytes; i < len(frame); i++ {
		frame[i] = byte(i)
	}
	if err := first.uplink.Send(frame); err != nil {
		t.Fatalf("send first peer frame: %v", err)
	}
	select {
	case received := <-second.frames:
		if !bytes.Equal(received, frame) {
			t.Fatal("second peer received a different voice frame")
		}
	case <-time.After(5 * time.Second):
		t.Fatal("second peer did not receive the first peer's voice frame")
	}
	select {
	case <-first.frames:
		t.Fatal("SFU looped the sender's voice frame back to its origin")
	case <-time.After(150 * time.Millisecond):
	}
	select {
	case frame := <-isolated.frames:
		t.Fatalf("SFU leaked a room frame into another session: %d bytes", len(frame))
	case <-time.After(150 * time.Millisecond):
	}
}

type testVoicePeer struct {
	pc *webrtc.PeerConnection
	uplink *webrtc.DataChannel
	downlinkOpen chan struct{}
	frames chan []byte
	signal *websocket.Conn
}

func (peer *testVoicePeer) close() {
	if peer.signal != nil {
		_ = peer.signal.Close(websocket.StatusNormalClosure, "test complete")
	}
	if peer.pc != nil {
		_ = peer.pc.Close()
	}
}

func connectTestVoicePeer(t *testing.T, endpoint, token string) *testVoicePeer {
	t.Helper()
	peer := &testVoicePeer{
		downlinkOpen: make(chan struct{}),
		frames: make(chan []byte, 8),
	}
	pc, err := webrtc.NewPeerConnection(webrtc.Configuration{})
	if err != nil {
		t.Fatal(err)
	}
	peer.pc = pc
	ordered := false
	maxRetransmits := uint16(0)
	uplink, err := pc.CreateDataChannel(voiceUplinkLabel, &webrtc.DataChannelInit{
		Ordered: &ordered, MaxRetransmits: &maxRetransmits,
	})
	if err != nil {
		peer.close()
		t.Fatal(err)
	}
	peer.uplink = uplink
	pc.OnDataChannel(func(channel *webrtc.DataChannel) {
		if channel.Label() != voiceDownlinkLabel {
			_ = channel.Close()
			return
		}
		channel.OnOpen(func() {
			select {
			case <-peer.downlinkOpen:
			default:
				close(peer.downlinkOpen)
			}
		})
		channel.OnMessage(func(message webrtc.DataChannelMessage) {
			if !message.IsString {
				peer.frames <- append([]byte(nil), message.Data...)
			}
		})
	})

	offer, err := pc.CreateOffer(nil)
	if err != nil {
		peer.close()
		t.Fatal(err)
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		peer.close()
		t.Fatal(err)
	}
	select {
	case <-gathered:
	case <-time.After(10 * time.Second):
		peer.close()
		t.Fatal("client ICE gathering timed out")
	}

	httpClient := &http.Client{Transport: &http.Transport{
		TLSClientConfig: &tls.Config{InsecureSkipVerify: true}, // test server uses a self-signed certificate
	}}
	signal, _, err := websocket.Dial(context.Background(), endpoint, &websocket.DialOptions{HTTPClient: httpClient})
	if err != nil {
		peer.close()
		t.Fatalf("dial local SFU signaling: %v", err)
	}
	peer.signal = signal
	join, err := json.Marshal(joinSignal{Type: "join", Token: token, Offer: *pc.LocalDescription()})
	if err != nil {
		peer.close()
		t.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	if err := signal.Write(ctx, websocket.MessageText, join); err != nil {
		peer.close()
		t.Fatalf("write join: %v", err)
	}
	kind, response, err := signal.Read(ctx)
	if err != nil || kind != websocket.MessageText {
		peer.close()
		t.Fatalf("read SFU answer: kind=%v err=%v", kind, err)
	}
	var answer serverSignal
	if err := json.Unmarshal(response, &answer); err != nil || answer.Type != "answer" || answer.Answer == nil {
		peer.close()
		t.Fatalf("unexpected SFU join response: %s (%v)", response, err)
	}
	if err := pc.SetRemoteDescription(*answer.Answer); err != nil {
		peer.close()
		t.Fatalf("set SFU answer: %v", err)
	}
	return peer
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
