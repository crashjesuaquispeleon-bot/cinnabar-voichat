package voice

import (
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"net/http"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
	"github.com/pion/webrtc/v4"
)

const (
	voiceUplinkLabel = "cinnabar-voice-uplink-v1"
	voiceDownlinkLabel = "cinnabar-voice-downlink-v1"
	voiceSampleRate = 16000
	voiceFrameDurationMS = 20
	voiceSamplesPerFrame = voiceSampleRate * voiceFrameDurationMS / 1000
	voicePCMBytesPerFrame = voiceSamplesPerFrame * 2
	voiceSequenceBytes = 8
	voiceFrameBytes = voiceSequenceBytes + voicePCMBytesPerFrame
	maxBufferedVoiceBytes = voiceFrameBytes * 5
	maxSignalMessageBytes = 64 << 10
	signalReadTimeout = 15 * time.Second
	iceGatherTimeout = 20 * time.Second
)

// SFUConfig configures the admission-gated WebRTC voice frame router.
// Configure at least one STUN/TURN server for internet connectivity; TURN is required
// for reliable connectivity across restrictive NATs and firewalls.
type SFUConfig struct {
	Admission VerifyConfig
	ICEServers []webrtc.ICEServer
	MaxParticipants int
}

// SFU accepts WebRTC offers only with a verified Cinnabar room admission.
type SFU struct {
	admission VerifyConfig
	peerConfig webrtc.Configuration
	registry *Registry

	mu sync.RWMutex
	rooms map[string]*sfuRoom
}

type sfuRoom struct {
	sessionID string
	peers map[string]*sfuPeer
}

type sfuPeer struct {
	sfu *SFU
	pc *webrtc.PeerConnection
	downlink *webrtc.DataChannel
	roomID string
	xuid string
	signalClose func() error
	pcClosed chan struct{}

	mu sync.Mutex
	admission Admission
	closed bool
	closeOnce sync.Once
	uplinkAttached atomic.Bool
	lastSequence atomic.Uint64
	droppedFrames atomic.Uint64
}

type joinSignal struct {
	Type string `json:"type"`
	Token string `json:"token"`
	Offer webrtc.SessionDescription `json:"offer"`
}

type clientSignal struct {
	Type string `json:"type"`
	Token string `json:"token"`
}

type serverSignal struct {
	Type string `json:"type"`
	Code string `json:"code,omitempty"`
	Answer *webrtc.SessionDescription `json:"answer,omitempty"`
	ExpiresAt int64 `json:"expires_at,omitempty"`
}

type signalRead struct {
	kind websocket.MessageType
	data []byte
	err error
}

// NewSFU creates a room-isolated WebRTC relay. The verifier contains only a public key;
// the room-issuing private key remains outside the media service.
func NewSFU(config SFUConfig) (*SFU, error) {
	if config.Admission.Issuer == "" || config.Admission.Audience == "" ||
		len(config.Admission.PublicKey) != 32 {
		return nil, ErrInvalidAdmission
	}
	if config.MaxParticipants == 0 {
		config.MaxParticipants = 8
	}
	registry, err := NewRegistry(config.MaxParticipants)
	if err != nil {
		return nil, err
	}
	config.Admission.PublicKey = append([]byte(nil), config.Admission.PublicKey...)
	return &SFU{
		admission: config.Admission,
		peerConfig: webrtc.Configuration{ICEServers: append([]webrtc.ICEServer(nil), config.ICEServers...)},
		registry: registry,
		rooms: make(map[string]*sfuRoom),
	}, nil
}

// ServeHTTP handles one WebSocket signaling connection. Mount this handler behind HTTPS
// at the operator's configured voice endpoint. Bearer tokens are sent only in messages,
// never in URLs, query strings, or log fields.
func (s *SFU) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	conn, err := websocket.Accept(w, r, nil)
	if err != nil {
		return
	}
	defer conn.CloseNow()
	conn.SetReadLimit(maxSignalMessageBytes)

	readCtx, cancel := context.WithTimeout(r.Context(), signalReadTimeout)
	kind, data, err := conn.Read(readCtx)
	cancel()
	if err != nil || kind != websocket.MessageText {
		return
	}
	var request joinSignal
	if decodeOneJSON(data, &request) != nil || request.Type != "join" ||
		request.Token == "" || request.Offer.Type != webrtc.SDPTypeOffer || request.Offer.SDP == "" {
		_ = writeSignal(r.Context(), conn, serverSignal{Type: "error", Code: "invalid_join"})
		return
	}

	peer, answer, err := s.join(r.Context(), request.Token, request.Offer, conn.CloseNow)
	if err != nil {
		_ = writeSignal(r.Context(), conn, serverSignal{Type: "error", Code: "admission_refused"})
		return
	}
	defer peer.close()
	if err := writeSignal(r.Context(), conn, serverSignal{Type: "answer", Answer: answer}); err != nil {
		return
	}

	// A single reader preserves WebSocket message order. Peer failure closes the socket
	// through signalClose, which unblocks this read.
	reads := make(chan signalRead, 1)
	go func() {
		for {
			kind, data, err := conn.Read(r.Context())
			select {
			case reads <- signalRead{kind: kind, data: data, err: err}:
			case <-r.Context().Done():
				return
			}
			if err != nil {
				return
			}
		}
	}()
	for {
		select {
		case <-r.Context().Done():
			return
		case <-peer.done():
			return
		case result := <-reads:
			if result.err != nil || result.kind != websocket.MessageText {
				return
			}
			var message clientSignal
			if decodeOneJSON(result.data, &message) != nil {
				return
			}
			switch message.Type {
			case "renew":
				if err := peer.renew(message.Token, time.Now()); err != nil {
					_ = writeSignal(r.Context(), conn, serverSignal{Type: "error", Code: "renewal_refused"})
					return
				}
				_ = writeSignal(r.Context(), conn, serverSignal{
					Type: "renewed", ExpiresAt: peer.currentAdmission().ExpiresAt.Unix(),
				})
			case "leave":
				return
			default:
				return
			}
		}
	}
}

func writeSignal(ctx context.Context, conn *websocket.Conn, value any) error {
	payload, err := json.Marshal(value)
	if err != nil {
		return err
	}
	ctx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	return conn.Write(ctx, websocket.MessageText, payload)
}

func (s *SFU) join(
	ctx context.Context,
	token string,
	offer webrtc.SessionDescription,
	closeSignal func() error,
) (*sfuPeer, *webrtc.SessionDescription, error) {
	admission, err := VerifyAdmission(token, s.admission, time.Now())
	if err != nil {
		return nil, nil, err
	}
	if err := s.registry.Join(admission, time.Now()); err != nil {
		return nil, nil, err
	}
	pc, err := webrtc.NewPeerConnection(s.peerConfig)
	if err != nil {
		s.registry.Revoke(admission.TokenID)
		return nil, nil, err
	}
	peer := &sfuPeer{
		sfu: s, pc: pc, roomID: admission.RoomID, xuid: admission.XUID,
		admission: admission, signalClose: closeSignal, pcClosed: make(chan struct{}),
	}
	ordered := false
	maxRetransmits := uint16(0)
	downlink, err := pc.CreateDataChannel(voiceDownlinkLabel, &webrtc.DataChannelInit{
		Ordered: &ordered, MaxRetransmits: &maxRetransmits,
	})
	if err != nil {
		peer.close()
		return nil, nil, err
	}
	peer.downlink = downlink
	pc.OnDataChannel(peer.acceptDataChannel)
	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed {
			peer.close()
		}
	})

	s.mu.Lock()
	room := s.rooms[admission.RoomID]
	if room == nil {
		room = &sfuRoom{sessionID: admission.SessionID, peers: make(map[string]*sfuPeer)}
		s.rooms[admission.RoomID] = room
	}
	if room.sessionID != admission.SessionID {
		s.mu.Unlock()
		peer.close()
		return nil, nil, ErrRoomSessionMismatch
	}
	room.peers[admission.XUID] = peer
	s.mu.Unlock()

	if err := pc.SetRemoteDescription(offer); err != nil {
		peer.close()
		return nil, nil, err
	}
	gathered := webrtc.GatheringCompletePromise(pc)
	answer, err := pc.CreateAnswer(nil)
	if err != nil {
		peer.close()
		return nil, nil, err
	}
	if err := pc.SetLocalDescription(answer); err != nil {
		peer.close()
		return nil, nil, err
	}
	timer := time.NewTimer(iceGatherTimeout)
	defer timer.Stop()
	select {
	case <-ctx.Done():
		peer.close()
		return nil, nil, ctx.Err()
	case <-timer.C:
		peer.close()
		return nil, nil, errors.New("voice: ICE gathering timed out")
	case <-gathered:
	}
	if pc.LocalDescription() == nil {
		peer.close()
		return nil, nil, errors.New("voice: missing local WebRTC description")
	}
	return peer, pc.LocalDescription(), nil
}

func (peer *sfuPeer) acceptDataChannel(channel *webrtc.DataChannel) {
	if channel.Label() != voiceUplinkLabel || !peer.uplinkAttached.CompareAndSwap(false, true) {
		_ = channel.Close()
		return
	}
	channel.OnMessage(func(message webrtc.DataChannelMessage) {
		if message.IsString {
			peer.droppedFrames.Add(1)
			return
		}
		sequence, ok := voiceFrameSequence(message.Data, peer.lastSequence.Load())
		if !ok || !peer.acceptSequence(sequence) {
			peer.droppedFrames.Add(1)
			return
		}
		// Pion owns the callback's byte slice; clone once before fan-out.
		peer.sfu.forward(peer, append([]byte(nil), message.Data...))
	})
}

func voiceFrameSequence(payload []byte, last uint64) (uint64, bool) {
	if len(payload) != voiceFrameBytes {
		return 0, false
	}
	sequence := binary.BigEndian.Uint64(payload[:voiceSequenceBytes])
	return sequence, sequence > last
}

func (peer *sfuPeer) acceptSequence(sequence uint64) bool {
	for {
		last := peer.lastSequence.Load()
		if sequence <= last {
			return false
		}
		if peer.lastSequence.CompareAndSwap(last, sequence) {
			return true
		}
	}
}

func (s *SFU) forward(sender *sfuPeer, frame []byte) {
	s.mu.RLock()
	room := s.rooms[sender.roomID]
	if room == nil || room.sessionID != sender.currentAdmission().SessionID {
		s.mu.RUnlock()
		return
	}
	targets := make([]*sfuPeer, 0, len(room.peers)-1)
	for _, peer := range room.peers {
		if peer != sender {
			targets = append(targets, peer)
		}
	}
	s.mu.RUnlock()

	for _, peer := range targets {
		if peer.downlink == nil || peer.downlink.ReadyState() != webrtc.DataChannelStateOpen ||
			peer.downlink.BufferedAmount() > maxBufferedVoiceBytes {
			continue
		}
		_ = peer.downlink.Send(frame)
	}
}

func (peer *sfuPeer) renew(token string, now time.Time) error {
	admission, err := VerifyAdmission(token, peer.sfu.admission, now)
	if err != nil {
		return err
	}
	peer.mu.Lock()
	defer peer.mu.Unlock()
	if peer.closed || admission.Subject != peer.admission.Subject ||
		admission.XUID != peer.admission.XUID || admission.RoomID != peer.admission.RoomID ||
		admission.SessionID != peer.admission.SessionID {
		return ErrInvalidAdmission
	}
	if err := peer.sfu.registry.Renew(admission, now); err != nil {
		return err
	}
	peer.admission = admission
	return nil
}

func (peer *sfuPeer) currentAdmission() Admission {
	peer.mu.Lock()
	defer peer.mu.Unlock()
	return peer.admission
}

func (peer *sfuPeer) done() <-chan struct{} {
	return peer.pcClosed
}

func (peer *sfuPeer) close() {
	peer.closeOnce.Do(func() {
		peer.mu.Lock()
		peer.closed = true
		admission := peer.admission
		peer.mu.Unlock()

		peer.sfu.mu.Lock()
		if room := peer.sfu.rooms[peer.roomID]; room != nil {
			if room.peers[peer.xuid] == peer {
				delete(room.peers, peer.xuid)
			}
			if len(room.peers) == 0 {
				delete(peer.sfu.rooms, peer.roomID)
			}
		}
		peer.sfu.mu.Unlock()
		peer.sfu.registry.Revoke(admission.TokenID)
		_ = peer.pc.Close()
		if peer.signalClose != nil {
			_ = peer.signalClose()
		}
		close(peer.pcClosed)
	})
}

// SweepExpired closes peers whose broker-issued admission lease was not renewed.
func (s *SFU) SweepExpired(now time.Time) int {
	expired := s.registry.Expire(now)
	for _, participant := range expired {
		s.mu.RLock()
		room := s.rooms[participant.RoomID]
		var peer *sfuPeer
		if room != nil {
			peer = room.peers[participant.XUID]
		}
		s.mu.RUnlock()
		if peer != nil {
			peer.close()
		}
	}
	return len(expired)
}

// CloseSession revokes and closes every peer admitted to the matching game-session UUID.
func (s *SFU) CloseSession(roomID, sessionID string) int {
	participants := s.registry.CloseSession(roomID, sessionID)
	var peers []*sfuPeer
	s.mu.RLock()
	if room := s.rooms[roomID]; room != nil && room.sessionID == sessionID {
		for _, participant := range participants {
			if peer := room.peers[participant.XUID]; peer != nil {
				peers = append(peers, peer)
			}
		}
	}
	s.mu.RUnlock()
	for _, peer := range peers {
		peer.close()
	}
	return len(peers)
}

// Close tears down every active PeerConnection and revokes its current admission.
func (s *SFU) Close() {
	s.mu.RLock()
	var peers []*sfuPeer
	for _, room := range s.rooms {
		for _, peer := range room.peers {
			peers = append(peers, peer)
		}
	}
	s.mu.RUnlock()
	for _, peer := range peers {
		peer.close()
	}
}
