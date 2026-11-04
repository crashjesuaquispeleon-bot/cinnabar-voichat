package voice

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"sync"
	"time"

	"github.com/hashimthearab/rust-mcbe/core/internal/bridgecontract"
	"github.com/hashimthearab/rust-mcbe/core/internal/streamnet"
	"github.com/pion/webrtc/v4"
)

// VoiceAdmissionSource is implemented by a trusted Cinnabar broker adapter.
// It must derive account identity and active partida membership server-side;
// it must never trust values sent over the local Rust IPC connection.
type VoiceAdmissionSource interface {
	IssueVoiceAdmissionToken(ctx context.Context) (string, error)
}

// VoiceBridgeConfig pins the media endpoint and the trusted admission-token source.
// The endpoint and ICE servers are operator configuration, never client request fields.
type VoiceBridgeConfig struct {
	Endpoint        string
	ICEServers      []webrtc.ICEServer
	AdmissionSource VoiceAdmissionSource
	HTTPClient      interface{ Do(*http.Request) (*http.Response, error) }
	Logger          *slog.Logger
	RenewInterval   time.Duration
}

type voiceIPCStatus struct {
	Status string `json:"status"`
	Code   string `json:"code,omitempty"`
}

type voiceIPCCommand struct {
	Kind      byte
	Frame     []byte
	Transmit  bool
	Close     bool
}

// parseVoiceIPCCommand validates the local one-byte tagged protocol before any
// network or media operation is attempted.
func parseVoiceIPCCommand(payload []byte) (voiceIPCCommand, error) {
	if len(payload) == 0 || len(payload) > bridgecontract.VoiceMaxFrameLen {
		return voiceIPCCommand{}, ErrVoiceFrameInvalid
	}
	command := voiceIPCCommand{Kind: payload[0]}
	switch command.Kind {
	case bridgecontract.VoiceIPCKindAudio:
		if len(payload) != bridgecontract.VoiceFrameBytes+1 {
			return voiceIPCCommand{}, ErrVoiceFrameInvalid
		}
		command.Frame = payload[1:]
	case bridgecontract.VoiceIPCKindTransmit:
		if len(payload) != 2 || payload[1] > 1 {
			return voiceIPCCommand{}, ErrVoiceFrameInvalid
		}
		command.Transmit = payload[1] == 1
	case bridgecontract.VoiceIPCKindClose:
		if len(payload) != 1 {
			return voiceIPCCommand{}, ErrVoiceFrameInvalid
		}
		command.Close = true
	default:
		return voiceIPCCommand{}, ErrVoiceFrameInvalid
	}
	return command, nil
}

// ServeBridge accepts Rust IPC connections one at a time. Disconnecting the IPC
// connection closes the WebRTC peer and revokes its current admission token.
func ServeBridge(ctx context.Context, listener net.Listener, config VoiceBridgeConfig) error {
	if listener == nil || !validVoiceEndpoint(config.Endpoint) || config.AdmissionSource == nil {
		if listener != nil {
			_ = listener.Close()
		}
		return errors.New("voice: bridge requires a WSS endpoint and trusted admission source")
	}
	defer listener.Close()
	logger := config.Logger
	if logger == nil {
		logger = slog.Default()
	}
	if config.RenewInterval <= 0 {
		config.RenewInterval = time.Minute
	}
	listenerDone := make(chan struct{})
	go func() {
		select {
		case <-ctx.Done():
			_ = listener.Close()
		case <-listenerDone:
		}
	}()
	defer close(listenerDone)

	for {
		connection, err := listener.Accept()
		if err != nil {
			if ctx.Err() != nil {
				return nil
			}
			return fmt.Errorf("voice: accept local IPC connection: %w", err)
		}
		err = serveVoiceIPCConnection(ctx, connection, config)
		if err != nil && ctx.Err() == nil {
			// Errors are connection-scoped. Never log bearer tokens or payloads.
			logger.Warn("voice IPC session ended", "error", err)
		}
		if ctx.Err() != nil {
			return nil
		}
	}
}

func serveVoiceIPCConnection(ctx context.Context, raw net.Conn, config VoiceBridgeConfig) error {
	defer raw.Close()
	ipc := streamnet.NewFramedConnWithMax(raw, bridgecontract.VoiceMaxFrameLen)

	token, err := config.AdmissionSource.IssueVoiceAdmissionToken(ctx)
	if err != nil || token == "" {
		_ = writeVoiceIPCStatus(ipc, voiceIPCStatus{Status: "disabled", Code: "admission_unavailable"})
		return errors.New("voice: trusted admission source did not issue a room token")
	}
	client, err := DialClient(ctx, ClientConfig{
		Endpoint: config.Endpoint, AdmissionToken: token,
		ICEServers: config.ICEServers, HTTPClient: config.HTTPClient,
	})
	if err != nil {
		_ = writeVoiceIPCStatus(ipc, voiceIPCStatus{Status: "disabled", Code: "voice_service_unavailable"})
		return errors.New("voice: could not join the authenticated media room")
	}
	defer client.Close()
	if err := writeVoiceIPCStatus(ipc, voiceIPCStatus{Status: "connected"}); err != nil {
		return err
	}

	sessionCtx, cancel := context.WithCancel(ctx)
	defer cancel()
	type commandResult struct {
		payload []byte
		err     error
	}
	commands := make(chan commandResult, 1)
	go func() {
		for {
			payload, err := ipc.ReadPacket()
			select {
			case commands <- commandResult{payload: payload, err: err}:
			case <-sessionCtx.Done():
				return
			}
			if err != nil {
				return
			}
		}
	}()

	remoteDone := make(chan error, 1)
	go func() {
		for {
			select {
			case <-sessionCtx.Done():
				remoteDone <- sessionCtx.Err()
				return
			case <-client.Done():
				remoteDone <- ErrVoiceClientClosed
				return
			case frame := <-client.ReceiveFrames():
				if len(frame) != bridgecontract.VoiceFrameBytes {
					continue
				}
				payload := make([]byte, bridgecontract.VoiceFrameBytes+1)
				payload[0] = bridgecontract.VoiceIPCKindRemoteAudio
				copy(payload[1:], frame)
				if _, err := ipc.Write(payload); err != nil {
					remoteDone <- err
					return
				}
			}
		}
	}()

	renewDone := make(chan error, 1)
	go func() {
		ticker := time.NewTicker(config.RenewInterval)
		defer ticker.Stop()
		for {
			select {
			case <-sessionCtx.Done():
				renewDone <- sessionCtx.Err()
				return
			case <-ticker.C:
				nextToken, err := config.AdmissionSource.IssueVoiceAdmissionToken(sessionCtx)
				if err != nil || nextToken == "" {
					client.Close()
					renewDone <- errors.New("voice: admission renewal unavailable")
					return
				}
				renewCtx, cancelRenew := context.WithTimeout(sessionCtx, 10*time.Second)
				err = client.RenewAdmission(renewCtx, nextToken)
				cancelRenew()
				if err != nil {
					client.Close()
					renewDone <- errors.New("voice: admission renewal rejected")
					return
				}
			}
		}
	}()

	for {
		select {
		case <-sessionCtx.Done():
			return sessionCtx.Err()
		case err := <-remoteDone:
			return err
		case err := <-renewDone:
			return err
		case result := <-commands:
			if result.err != nil {
				return result.err
			}
			command, err := parseVoiceIPCCommand(result.payload)
			if err != nil {
				return err
			}
			switch command.Kind {
			case bridgecontract.VoiceIPCKindAudio:
				if err := client.SendFrame(command.Frame); err != nil {
					switch err {
					case ErrVoiceTransmitDisabled, ErrVoiceBackpressure:
						// Drop locally suppressed or congested frames; never retain audio.
						continue
					default:
						return err
					}
				}
			case bridgecontract.VoiceIPCKindTransmit:
				transmitCtx, cancelTransmit := context.WithTimeout(sessionCtx, 5*time.Second)
				err := client.SetTransmitAllowed(transmitCtx, command.Transmit)
				cancelTransmit()
				if err != nil {
					return errors.New("voice: server did not acknowledge the transmit gate")
				}
				status := "muted"
				if command.Transmit {
					status = "transmitting"
				}
				if err := writeVoiceIPCStatus(ipc, voiceIPCStatus{Status: status}); err != nil {
					return err
				}
			case bridgecontract.VoiceIPCKindClose:
				return nil
			}
		}
	}
}

func writeVoiceIPCStatus(ipc *streamnet.FramedConn, status voiceIPCStatus) error {
	encoded, err := json.Marshal(status)
	if err != nil {
		return err
	}
	if len(encoded)+1 > bridgecontract.VoiceMaxFrameLen {
		return ErrVoiceFrameInvalid
	}
	payload := make([]byte, len(encoded)+1)
	payload[0] = bridgecontract.VoiceIPCKindStatus
	copy(payload[1:], encoded)
	_, err = ipc.Write(payload)
	return err
}

// Keep one local connection's write path serialized even when downlink and
// status messages are produced concurrently by independent goroutines.
var _ sync.Locker = (*sync.Mutex)(nil)
