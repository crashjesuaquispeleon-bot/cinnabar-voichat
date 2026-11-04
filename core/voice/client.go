package voice

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"net/url"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
	"github.com/pion/webrtc/v4"
)

var (
	ErrVoiceEndpointInvalid = errors.New("voice: endpoint must be a wss URL without query credentials")
	ErrVoiceClientClosed = errors.New("voice: client is closed")
	ErrVoiceTransmitDisabled = errors.New("voice: microphone transmission is not permitted")
	ErrVoiceFrameInvalid = errors.New("voice: invalid PCM frame")
	ErrVoiceBackpressure = errors.New("voice: uplink is backpressured")
)

const (
	clientReceiveQueueSize = 32
	clientSetupTimeout = iceGatherTimeout + 10*time.Second
)

// ClientConfig contains one short-lived broker admission token and operator-supplied
// ICE settings. The token is sent only in the first WSS message, never in the URL.
type ClientConfig struct {
	Endpoint string
	AdmissionToken string
	ICEServers []webrtc.ICEServer
	HTTPClient *http.Client
}

// Client is the native Go WebRTC edge used by the Cinnabar core. Rust can pass capture
// frames to SendFrame and drain ReceiveFrames through a dedicated local IPC adapter.
type Client struct {
	pc *webrtc.PeerConnection
	signal *websocket.Conn
	uplink *webrtc.DataChannel

	incoming chan []byte
	done chan struct{}
	ready chan struct{}

	mu sync.Mutex
	closed bool
	uplinkOpen bool
	downlinkOpen bool
	readySignaled bool
	downlinkAttached atomic.Bool
	transmitAllowed atomic.Bool
	lastSequence atomic.Uint64
	droppedFrames atomic.Uint64

	signalMu sync.Mutex
	closeOnce sync.Once
}

// DialClient creates a WebRTC session using a broker-signed admission token. The server
// verifies the token and derives all room/participant identity from its signed claims.
func DialClient(ctx context.Context, config ClientConfig) (*Client, error) {
	if !validVoiceEndpoint(config.Endpoint) || config.AdmissionToken == "" ||
		len(config.AdmissionToken) > maxAdmissionTokenBytes {
		return nil, ErrInvalidAdmission
	}
	peerConfig := webrtc.Configuration{ICEServers: append([]webrtc.ICEServer(nil), config.ICEServers...)}
	pc, err := webrtc.NewPeerConnection(peerConfig)
	if err != nil {
		return nil, err
	}
	client := &Client{
		pc: pc,
		incoming: make(chan []byte, clientReceiveQueueSize),
		done: make(chan struct{}),
		ready: make(chan struct{}),
	}
	fail := func(err error) (*Client, error) {
		client.Close()
		return nil, err
	}

	uplinkOrdered := false
	uplinkMaxRetransmits := uint16(0)
	uplink, err := pc.CreateDataChannel(voiceUplinkLabel, &webrtc.DataChannelInit{
		Ordered: &uplinkOrdered, MaxRetransmits: &uplinkMaxRetransmits,
	})
	if err != nil {
		return fail(err)
	}
	client.uplink = uplink
	uplink.OnOpen(func() { client.markChannelOpen(voiceUplinkLabel) })
	uplink.OnClose(func() { go client.Close() })
	pc.OnDataChannel(client.acceptDownlink)
	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
			go client.Close()
		}
	})

	offer, err := pc.CreateOffer(nil)
	if err != nil {
		return fail(err)
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	if err := pc.SetLocalDescription(offer); err != nil {
		return fail(err)
	}
	gatherCtx, cancelGather := context.WithTimeout(ctx, iceGatherTimeout)
	defer cancelGather()
	select {
	case <-ctx.Done():
		return fail(ctx.Err())
	case <-gatherCtx.Done():
		return fail(errors.New("voice: client ICE gathering timed out"))
	case <-gathered:
	}

	signal, _, err := websocket.Dial(ctx, config.Endpoint, &websocket.DialOptions{HTTPClient: config.HTTPClient})
	if err != nil {
		return fail(err)
	}
	client.signal = signal
	signal.SetReadLimit(maxSignalMessageBytes)
	joinPayload, err := json.Marshal(joinSignal{
		Type: "join", Token: config.AdmissionToken, Offer: *pc.LocalDescription(),
	})
	if err != nil {
		return fail(err)
	}
	if err := signal.Write(ctx, websocket.MessageText, joinPayload); err != nil {
		return fail(err)
	}
	answerCtx, cancelAnswer := context.WithTimeout(ctx, clientSetupTimeout)
	defer cancelAnswer()
	kind, response, err := signal.Read(answerCtx)
	if err != nil {
		return fail(err)
	}
	if kind != websocket.MessageText {
		return fail(ErrInvalidAdmission)
	}
	var answer serverSignal
	if err := decodeOneJSON(response, &answer); err != nil || answer.Type != "answer" || answer.Answer == nil {
		return fail(ErrInvalidAdmission)
	}
	if err := pc.SetRemoteDescription(*answer.Answer); err != nil {
		return fail(err)
	}
	if err := client.WaitReady(answerCtx); err != nil {
		return fail(err)
	}
	return client, nil
}

func validVoiceEndpoint(endpoint string) bool {
	parsed, err := url.Parse(endpoint)
	return err == nil && parsed.Scheme == "wss" && parsed.Host != "" &&
		parsed.User == nil && parsed.RawQuery == "" && parsed.Fragment == ""
}

func (client *Client) acceptDownlink(channel *webrtc.DataChannel) {
	if channel.Label() != voiceDownlinkLabel || !client.downlinkAttached.CompareAndSwap(false, true) {
		_ = channel.Close()
		return
	}
	channel.OnOpen(func() { client.markChannelOpen(voiceDownlinkLabel) })
	channel.OnClose(func() { go client.Close() })
	channel.OnMessage(func(message webrtc.DataChannelMessage) {
		if message.IsString || len(message.Data) != voiceRemoteFrameBytes {
			client.droppedFrames.Add(1)
			return
		}
		frame := append([]byte(nil), message.Data...)
		select {
		case client.incoming <- frame:
		case <-client.done:
		default:
			client.droppedFrames.Add(1)
		}
	})
}

func (client *Client) markChannelOpen(label string) {
	client.mu.Lock()
	defer client.mu.Unlock()
	if client.closed {
		return
	}
	switch label {
	case voiceUplinkLabel:
		client.uplinkOpen = true
	case voiceDownlinkLabel:
		client.downlinkOpen = true
	}
	if client.uplinkOpen && client.downlinkOpen && !client.readySignaled {
		close(client.ready)
		client.readySignaled = true
	}
}

// WaitReady blocks until the unordered uplink and SFU-created downlink have opened.
func (client *Client) WaitReady(ctx context.Context) error {
	select {
	case <-ctx.Done():
		return ctx.Err()
	case <-client.done:
		return ErrVoiceClientClosed
	case <-client.ready:
		return nil
	}
}

// SetTransmitAllowed updates the local gate immediately on release and waits for
// the SFU's reliable signaling acknowledgement before opening the local gate on press.
// If acknowledgement fails, the connection is closed so the server cannot remain open.
func (client *Client) SetTransmitAllowed(ctx context.Context, allowed bool) error {
	client.signalMu.Lock()
	defer client.signalMu.Unlock()

	select {
	case <-client.done:
		return ErrVoiceClientClosed
	default:
	}
	if !allowed {
		client.transmitAllowed.Store(false)
	}
	if client.signal == nil {
		client.Close()
		return ErrVoiceClientClosed
	}
	payload, err := json.Marshal(clientSignal{Type: "transmit", Allowed: allowed})
	if err != nil {
		client.Close()
		return err
	}
	if err := client.signal.Write(ctx, websocket.MessageText, payload); err != nil {
		client.Close()
		return err
	}
	ackCtx, cancel := context.WithTimeout(ctx, signalReadTimeout)
	defer cancel()
	kind, response, err := client.signal.Read(ackCtx)
	if err != nil || kind != websocket.MessageText {
		client.Close()
		return ErrVoiceClientClosed
	}
	var acknowledgement serverSignal
	if err := decodeOneJSON(response, &acknowledgement); err != nil ||
		acknowledgement.Type != "transmit" || acknowledgement.Allowed != allowed {
		client.Close()
		return ErrInvalidAdmission
	}
	if allowed {
		client.transmitAllowed.Store(true)
	}
	return nil
}

// SendFrame sends exactly one 20 ms mono PCM16 frame. The caller must apply the local
// microphone policy before calling; this client applies a second local gate and the SFU
// independently enforces the reliable remote gate.
func (client *Client) SendFrame(frame []byte) error {
	if !client.transmitAllowed.Load() {
		return ErrVoiceTransmitDisabled
	}
	if len(frame) != voiceFrameBytes {
		return ErrVoiceFrameInvalid
	}
	select {
	case <-client.done:
		return ErrVoiceClientClosed
	default:
	}
	if client.uplink == nil || client.uplink.ReadyState() != webrtc.DataChannelStateOpen {
		return ErrVoiceClientClosed
	}
	if client.uplink.BufferedAmount() > maxBufferedVoiceBytes {
		client.droppedFrames.Add(1)
		return ErrVoiceBackpressure
	}
	sequence, ok := voiceFrameSequence(frame, client.lastSequence.Load())
	if !ok || !client.acceptSequence(sequence) {
		return ErrVoiceFrameInvalid
	}
	if err := client.uplink.Send(append([]byte(nil), frame...)); err != nil {
		client.droppedFrames.Add(1)
		return err
	}
	return nil
}

func (client *Client) acceptSequence(sequence uint64) bool {
	for {
		last := client.lastSequence.Load()
		if sequence <= last {
			return false
		}
		if client.lastSequence.CompareAndSwap(last, sequence) {
			return true
		}
	}
}

// ReceiveFrames returns a bounded stream of frames from the authenticated room.
// The consumer must also observe Done because the channel remains open to avoid a send/close race.
func (client *Client) ReceiveFrames() <-chan []byte {
	return client.incoming
}

func (client *Client) Done() <-chan struct{} {
	return client.done
}

func (client *Client) DroppedFrames() uint64 {
	return client.droppedFrames.Load()
}

// RenewAdmission requests a fresh short-lived token from the trusted broker, then submits
// it to the signaling connection. It must represent the same account, XUID, room and partida.
func (client *Client) RenewAdmission(ctx context.Context, token string) error {
	if token == "" || len(token) > maxAdmissionTokenBytes {
		return ErrInvalidAdmission
	}
	client.signalMu.Lock()
	defer client.signalMu.Unlock()
	select {
	case <-client.done:
		return ErrVoiceClientClosed
	default:
	}
	if client.signal == nil {
		return ErrVoiceClientClosed
	}
	payload, err := json.Marshal(clientSignal{Type: "renew", Token: token})
	if err != nil {
		return err
	}
	if err := client.signal.Write(ctx, websocket.MessageText, payload); err != nil {
		client.Close()
		return err
	}
	answerCtx, cancel := context.WithTimeout(ctx, signalReadTimeout)
	defer cancel()
	kind, response, err := client.signal.Read(answerCtx)
	if err != nil {
		client.Close()
		return err
	}
	if kind != websocket.MessageText {
		client.Close()
		return ErrInvalidAdmission
	}
	var answer serverSignal
	if err := decodeOneJSON(response, &answer); err != nil || answer.Type != "renewed" {
		client.Close()
		return ErrInvalidAdmission
	}
	return nil
}

// Close disables transmission, closes signaling and tears down the PeerConnection once.
func (client *Client) Close() {
	client.closeOnce.Do(func() {
		client.mu.Lock()
		client.closed = true
		client.mu.Unlock()
		client.transmitAllowed.Store(false)
		close(client.done)
		if client.signal != nil {
			_ = client.signal.Close(websocket.StatusNormalClosure, "voice client closed")
		}
		if client.pc != nil {
			_ = client.pc.Close()
		}
	})
}
