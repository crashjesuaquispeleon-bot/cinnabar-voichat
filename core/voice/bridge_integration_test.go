package voice

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/google/uuid"
	"github.com/hashimthearab/rust-mcbe/core/internal/bridgecontract"
	"github.com/hashimthearab/rust-mcbe/core/internal/streamnet"
)

type fixedVoiceAdmissionSource struct {
	token string
}

func (source fixedVoiceAdmissionSource) IssueVoiceAdmissionToken(context.Context) (string, error) {
	return source.token, nil
}

func readVoiceIPCStatus(t *testing.T, ipc *streamnet.FramedConn, want string) {
	t.Helper()
	payload, err := ipc.ReadPacket()
	if err != nil {
		t.Fatalf("read voice IPC status %q: %v", want, err)
	}
	if len(payload) < 2 || payload[0] != bridgecontract.VoiceIPCKindStatus {
		t.Fatalf("voice IPC status payload = %v, want status frame", payload)
	}
	var status voiceIPCStatus
	if err := json.Unmarshal(payload[1:], &status); err != nil {
		t.Fatalf("decode voice IPC status: %v", err)
	}
	if status.Status != want {
		t.Fatalf("voice IPC status = %q, want %q (code %q)", status.Status, want, status.Code)
	}
}

func bridgeTestFrame(sequence uint64, sample byte) []byte {
	frame := make([]byte, bridgecontract.VoiceFrameBytes)
	binary.BigEndian.PutUint64(frame[:8], sequence)
	for index := 8; index < len(frame); index++ {
		frame[index] = sample
	}
	return frame
}

func waitForVoiceParticipants(t *testing.T, registry *Registry, roomID string, count int) {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	for time.Now().Before(deadline) {
		if got := len(registry.Participants(roomID)); got == count {
			return
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatalf("voice participants = %d, want %d", len(registry.Participants(roomID)), count)
}

// TestServeBridgeRelaysBothDirectionsAndHonoursTheAcknowledgedTransmitGate exercises
// the native local IPC bridge against a real in-process Pion SFU and a second peer.
// The admission issuer is deliberately a test-only fixed token source; production must
// inject a broker that proves account identity and authoritative partida membership.
func TestServeBridgeRelaysBothDirectionsAndHonoursTheAcknowledgedTransmitGate(t *testing.T) {
	now := time.Now()
	privateKey, verify := verifier(t)
	roomID, sessionID := uuid.NewString(), uuid.NewString()

	bridgeClaims := claimsAt(now)
	bridgeClaims.RoomID, bridgeClaims.SessionID, bridgeClaims.XUID = roomID, sessionID, "10001"
	bridgeToken := signedToken(t, privateKey, bridgeClaims)

	peerClaims := claimsAt(now)
	peerClaims.RoomID, peerClaims.SessionID, peerClaims.XUID = roomID, sessionID, "10002"
	peerToken := signedToken(t, privateKey, peerClaims)

	sfu, err := NewSFU(SFUConfig{Admission: verify, MaxParticipants: 4})
	if err != nil {
		t.Fatal(err)
	}
	defer sfu.Close()

	signaling := httptest.NewTLSServer(http.HandlerFunc(sfu.ServeHTTP))
	defer signaling.Close()
	endpoint := "wss" + strings.TrimPrefix(signaling.URL, "https")

	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	bridgeCtx, cancelBridge := context.WithTimeout(context.Background(), 45*time.Second)
	defer cancelBridge()
	bridgeDone := make(chan error, 1)
	go func() {
		bridgeDone <- ServeBridge(bridgeCtx, listener, VoiceBridgeConfig{
			Endpoint: endpoint,
			AdmissionSource: fixedVoiceAdmissionSource{token: bridgeToken},
			HTTPClient: signaling.Client(),
			RenewInterval: time.Hour,
		})
	}()

	local, err := net.DialTimeout("tcp", listener.Addr().String(), 2*time.Second)
	if err != nil {
		t.Fatalf("connect local voice IPC: %v", err)
	}
	ipc := streamnet.NewFramedConnWithMax(local, bridgecontract.VoiceMaxFrameLen)
	defer ipc.Close()
	_ = local.SetDeadline(time.Now().Add(35 * time.Second))

	readVoiceIPCStatus(t, ipc, "connected")

	peer, err := DialClient(bridgeCtx, ClientConfig{
		Endpoint: endpoint,
		AdmissionToken: peerToken,
		HTTPClient: signaling.Client(),
	})
	if err != nil {
		t.Fatalf("connect second WebRTC peer: %v", err)
	}
	defer peer.Close()
	if err := peer.SetTransmitAllowed(bridgeCtx, true); err != nil {
		t.Fatalf("enable second peer transmit: %v", err)
	}

	if _, err := ipc.Write([]byte{bridgecontract.VoiceIPCKindTransmit, 1}); err != nil {
		t.Fatalf("open bridge PTT gate: %v", err)
	}
	readVoiceIPCStatus(t, ipc, "transmitting")

	outbound := bridgeTestFrame(1, 0x35)
	outboundCommand := append([]byte{bridgecontract.VoiceIPCKindAudio}, outbound...)
	if _, err := ipc.Write(outboundCommand); err != nil {
		t.Fatalf("send local microphone frame: %v", err)
	}
	select {
	case received := <-peer.ReceiveFrames():
		if string(received) != string(outbound) {
			t.Fatal("second peer received a different frame from the local IPC uplink")
		}
	case <-bridgeCtx.Done():
		t.Fatal("second peer did not receive the local IPC uplink frame")
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for the local IPC uplink frame")
	}

	inbound := bridgeTestFrame(1, 0x6a)
	if err := peer.SendFrame(inbound); err != nil {
		t.Fatalf("send second peer frame: %v", err)
	}
	remotePayload, err := ipc.ReadPacket()
	if err != nil {
		t.Fatalf("read remote audio from local IPC: %v", err)
	}
	if len(remotePayload) != bridgecontract.VoiceFrameBytes+1 ||
		remotePayload[0] != bridgecontract.VoiceIPCKindRemoteAudio {
		t.Fatalf("remote IPC payload has invalid kind or length: %d bytes", len(remotePayload))
	}
	if string(remotePayload[1:]) != string(inbound) {
		t.Fatal("local IPC received a different frame from the second peer")
	}

	if _, err := ipc.Write([]byte{bridgecontract.VoiceIPCKindTransmit, 0}); err != nil {
		t.Fatalf("close bridge PTT gate: %v", err)
	}
	readVoiceIPCStatus(t, ipc, "muted")

	suppressed := bridgeTestFrame(2, 0x7f)
	if _, err := ipc.Write(append([]byte{bridgecontract.VoiceIPCKindAudio}, suppressed...)); err != nil {
		t.Fatalf("send frame while muted: %v", err)
	}
	select {
	case leaked := <-peer.ReceiveFrames():
		t.Fatalf("muted bridge leaked %d bytes to the room", len(leaked))
	case <-time.After(200 * time.Millisecond):
	}

	if _, err := ipc.Write([]byte{bridgecontract.VoiceIPCKindClose}); err != nil {
		t.Fatalf("close bridge IPC session: %v", err)
	}
	waitForVoiceParticipants(t, sfu.registry, roomID, 1)

	cancelBridge()
	select {
	case err := <-bridgeDone:
		if err != nil {
			t.Fatalf("stop voice bridge: %v", err)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("voice bridge did not stop after context cancellation")
	}
	if len(sfu.registry.Participants(roomID)) != 1 {
		t.Fatal("closing the local IPC session should revoke only its peer admission")
	}
}

func TestServeBridgeFailsClosedWithoutTrustedAdmissionSource(t *testing.T) {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	err = ServeBridge(context.Background(), listener, VoiceBridgeConfig{Endpoint: "wss://voice.example"})
	if err == nil || !strings.Contains(err.Error(), "trusted admission source") {
		t.Fatalf("missing trusted admission source error = %v", err)
	}
	if _, err := listener.Accept(); !errors.Is(err, net.ErrClosed) {
		t.Fatalf("listener was not closed after invalid configuration: %v", err)
	}
}
