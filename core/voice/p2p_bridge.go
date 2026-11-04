package voice

import (
	"context"
	"encoding/json"
	"errors"
	"net"
	"time"

	"github.com/hashimthearab/rust-mcbe/core/internal/bridgecontract"
	"github.com/hashimthearab/rust-mcbe/core/internal/streamnet"
	"github.com/pion/webrtc/v4"
)

// P2PSignaler is implemented by the authenticated Cinnabar API client.
type P2PSignaler interface {
	VoicePeerSession(context.Context) (string, bool, error)
	SendVoiceSignal(context.Context, string, string, string) error
	ReceiveVoiceSignal(context.Context) (string, string, string, error)
}

func serveP2PVoiceIPCConnection(ctx context.Context, raw net.Conn, iceServers []webrtc.ICEServer, signaler P2PSignaler) error {
	defer raw.Close()
	ipc := streamnet.NewFramedConnWithMax(raw, bridgecontract.VoiceMaxFrameLen)
	sessionID, offerer, err := signaler.VoicePeerSession(ctx)
	if err != nil { _ = writeVoiceIPCStatus(ipc, voiceIPCStatus{Status:"disabled", Code:"peer_unavailable"}); return errors.New("voice: no authorized two-player room is available") }
	peer, err := NewP2PPeer(iceServers, offerer)
	if err != nil { _ = writeVoiceIPCStatus(ipc, voiceIPCStatus{Status:"disabled", Code:"peer_unavailable"}); return errors.New("voice: could not create P2P peer") }
	defer peer.Close()

	signalCtx, cancel := context.WithTimeout(ctx, 35*time.Second)
	defer cancel()
	if offerer {
		offer, err := peer.CreateOffer(signalCtx)
		if err != nil { return err }
		encoded, err := json.Marshal(offer)
		if err != nil { return err }
		if len(encoded) > 8192 { return errors.New("voice: P2P offer exceeds broker signaling limit") }
		if err := signaler.SendVoiceSignal(signalCtx, sessionID, "offer", string(encoded)); err != nil { return errors.New("voice: broker rejected P2P offer") }
		remoteSession, kind, payload, err := signaler.ReceiveVoiceSignal(signalCtx)
		if err != nil { return errors.New("voice: waiting for P2P answer failed") }
		if remoteSession != sessionID || kind != "answer" { return errors.New("voice: received an unexpected P2P signal") }
		var answer webrtc.SessionDescription
		if json.Unmarshal([]byte(payload), &answer) != nil || peer.AcceptAnswer(answer) != nil { return errors.New("voice: invalid P2P answer") }
	} else {
		remoteSession, kind, payload, err := signaler.ReceiveVoiceSignal(signalCtx)
		if err != nil { return errors.New("voice: waiting for P2P offer failed") }
		if remoteSession != sessionID || kind != "offer" { return errors.New("voice: received an unexpected P2P signal") }
		var offer webrtc.SessionDescription
		if json.Unmarshal([]byte(payload), &offer) != nil { return errors.New("voice: invalid P2P offer") }
		answer, err := peer.AcceptOffer(signalCtx, offer)
		if err != nil { return errors.New("voice: could not accept P2P offer") }
		encoded, err := json.Marshal(answer)
		if err != nil { return err }
		if len(encoded) > 8192 { return errors.New("voice: P2P answer exceeds broker signaling limit") }
		if err := signaler.SendVoiceSignal(signalCtx, sessionID, "answer", string(encoded)); err != nil { return errors.New("voice: broker rejected P2P answer") }
	}
	if err := peer.WaitReady(signalCtx); err != nil { return errors.New("voice: direct P2P connection did not become ready") }
	if err := writeVoiceIPCStatus(ipc, voiceIPCStatus{Status:"connected"}); err != nil { return err }

	sessionCtx, stop := context.WithCancel(ctx)
	defer stop()
	type commandResult struct { payload []byte; err error }
	commands := make(chan commandResult, 1)
	go func() {
		for { payload, readErr := ipc.ReadPacket(); select {
			case commands <- commandResult{payload:payload, err:readErr}:
			case <-sessionCtx.Done(): return
		}; if readErr != nil { return } }
	}()
	remoteDone := make(chan error, 1)
	go func() {
		for { select {
		case <-sessionCtx.Done(): remoteDone <- sessionCtx.Err(); return
		case <-peer.Done(): remoteDone <- ErrVoiceClientClosed; return
		case frame := <-peer.ReceiveFrames():
			if len(frame) != voiceRemoteFrameBytes { continue }
			payload := make([]byte, voiceRemoteFrameBytes+1)
			payload[0] = bridgecontract.VoiceIPCKindRemoteAudio
			copy(payload[1:], frame)
			if _, writeErr := ipc.Write(payload); writeErr != nil { remoteDone <- writeErr; return }
		} }
	}()
	membership := time.NewTicker(15*time.Second)
	defer membership.Stop()
	for {
		select {
		case <-sessionCtx.Done(): return sessionCtx.Err()
		case err := <-remoteDone: return err
		case <-membership.C:
			checkCtx, cancelCheck := context.WithTimeout(sessionCtx, 5*time.Second)
			currentSession, currentOfferer, checkErr := signaler.VoicePeerSession(checkCtx)
			cancelCheck()
			if checkErr != nil || currentSession != sessionID || currentOfferer != offerer {
				peer.Close()
				return errors.New("voice: P2P room membership was revoked or changed")
			}
		case result := <-commands:
			if result.err != nil { return result.err }
			command, parseErr := parseVoiceIPCCommand(result.payload)
			if parseErr != nil { return parseErr }
			switch command.Kind {
			case bridgecontract.VoiceIPCKindAudio:
				if sendErr := peer.SendFrame(command.Frame); sendErr != nil {
					if sendErr == ErrVoiceTransmitDisabled || sendErr == ErrVoiceBackpressure { continue }
					return sendErr
				}
			case bridgecontract.VoiceIPCKindTransmit:
				peer.SetTransmitAllowed(command.Transmit)
				status := "muted"
				if command.Transmit { status = "transmitting" }
				if statusErr := writeVoiceIPCStatus(ipc, voiceIPCStatus{Status:status}); statusErr != nil { return statusErr }
			case bridgecontract.VoiceIPCKindClose:
				return nil
			}
		}
	}
}
