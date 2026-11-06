package voice

import (
	"context"
	"errors"
	"net"
	"testing"
	"time"

	"github.com/hashimthearab/rust-mcbe/core/internal/bridgecontract"
	"github.com/hashimthearab/rust-mcbe/core/internal/streamnet"
)

type p2pTestSignal struct {
	sessionID string
	kind      string
	payload   string
}

type pairedP2PSignaler struct {
	sessionID string
	offerer   bool
	incoming  chan p2pTestSignal
	peer      *pairedP2PSignaler
}

func (s *pairedP2PSignaler) VoicePeerSession(context.Context) (string, bool, error) {
	return s.sessionID, s.offerer, nil
}

func (s *pairedP2PSignaler) SendVoiceSignal(ctx context.Context, sessionID, kind, payload string) error {
	if sessionID != s.sessionID {
		return ErrVoiceFrameInvalid
	}
	select {
	case s.peer.incoming <- p2pTestSignal{sessionID: sessionID, kind: kind, payload: payload}:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}

func (s *pairedP2PSignaler) ReceiveVoiceSignal(ctx context.Context) (string, string, string, error) {
	select {
	case signal := <-s.incoming:
		return signal.sessionID, signal.kind, signal.payload, nil
	case <-ctx.Done():
		return "", "", "", ctx.Err()
	}
}

func TestP2PBridgeNegotiatesAndRelaysAudioOverLocalIPC(t *testing.T) {
	ctx, cancel := context.WithTimeout(context.Background(), 35*time.Second)
	defer cancel()

	sessionID := "00112233-4455-6677-8899-aabbccddeeff"
	hostSignal := &pairedP2PSignaler{
		sessionID: sessionID,
		offerer:   true,
		incoming:  make(chan p2pTestSignal, 4),
	}
	guestSignal := &pairedP2PSignaler{
		sessionID: sessionID,
		offerer:   false,
		incoming:  make(chan p2pTestSignal, 4),
	}
	hostSignal.peer, guestSignal.peer = guestSignal, hostSignal

	hostServer, hostClient := net.Pipe()
	guestServer, guestClient := net.Pipe()
	hostIPC := streamnet.NewFramedConnWithMax(hostClient, bridgecontract.VoiceMaxFrameLen)
	guestIPC := streamnet.NewFramedConnWithMax(guestClient, bridgecontract.VoiceMaxFrameLen)
	defer hostIPC.Close()
	defer guestIPC.Close()

	hostDone := make(chan error, 1)
	guestDone := make(chan error, 1)
	go func() {
		hostDone <- serveP2PVoiceIPCConnection(ctx, hostServer, nil, hostSignal)
	}()
	go func() {
		guestDone <- serveP2PVoiceIPCConnection(ctx, guestServer, nil, guestSignal)
	}()

	readVoiceIPCStatus(t, hostIPC, "connected")
	readVoiceIPCStatus(t, guestIPC, "connected")

	if _, err := hostIPC.Write([]byte{bridgecontract.VoiceIPCKindTransmit, 1}); err != nil {
		t.Fatal(err)
	}
	readVoiceIPCStatus(t, hostIPC, "transmitting")
	if _, err := guestIPC.Write([]byte{bridgecontract.VoiceIPCKindTransmit, 1}); err != nil {
		t.Fatal(err)
	}
	readVoiceIPCStatus(t, guestIPC, "transmitting")

	hostFrame := bridgeTestFrame(1, 0x31)
	if _, err := hostIPC.Write(append([]byte{bridgecontract.VoiceIPCKindAudio}, hostFrame...)); err != nil {
		t.Fatal(err)
	}
	guestRemote, err := guestIPC.ReadPacket()
	if err != nil {
		t.Fatalf("guest read remote audio: %v", err)
	}
	if len(guestRemote) != voiceRemoteFrameBytes+1 ||
		guestRemote[0] != bridgecontract.VoiceIPCKindRemoteAudio ||
		string(guestRemote[1+voiceSourceIDBytes:]) != string(hostFrame) {
		t.Fatal("guest IPC received an invalid host audio frame")
	}

	guestFrame := bridgeTestFrame(1, 0x62)
	if _, err := guestIPC.Write(append([]byte{bridgecontract.VoiceIPCKindAudio}, guestFrame...)); err != nil {
		t.Fatal(err)
	}
	hostRemote, err := hostIPC.ReadPacket()
	if err != nil {
		t.Fatalf("host read remote audio: %v", err)
	}
	if len(hostRemote) != voiceRemoteFrameBytes+1 ||
		hostRemote[0] != bridgecontract.VoiceIPCKindRemoteAudio ||
		string(hostRemote[1+voiceSourceIDBytes:]) != string(guestFrame) {
		t.Fatal("host IPC received an invalid guest audio frame")
	}

	if _, err := hostIPC.Write([]byte{bridgecontract.VoiceIPCKindTransmit, 0}); err != nil {
		t.Fatal(err)
	}
	readVoiceIPCStatus(t, hostIPC, "muted")
	if _, err := hostIPC.Write([]byte{bridgecontract.VoiceIPCKindClose}); err != nil {
		t.Fatal(err)
	}

	select {
	case err := <-hostDone:
		if err != nil {
			t.Fatalf("host bridge close: %v", err)
		}
	case <-ctx.Done():
		t.Fatal("host bridge did not close")
	}

	// Closing one peer must tear down the other bridge as a remote disconnect.
	// Its IPC endpoint is closed by the bridge, so the test must not write to it.
	select {
	case err := <-guestDone:
		if !errors.Is(err, ErrVoiceClientClosed) {
			t.Fatalf("guest bridge should stop on remote peer close, got: %v", err)
		}
	case <-ctx.Done():
		t.Fatal("guest bridge did not close after host disconnect")
	}
}
