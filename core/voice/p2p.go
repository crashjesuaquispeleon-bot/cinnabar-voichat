package voice

import (
	"context"
	"errors"
	"sync"
	"sync/atomic"
	"time"

	"github.com/google/uuid"
	"github.com/pion/webrtc/v4"
)

const p2pDataChannelLabel = "cinnabar-voice-p2p-v1"

var ErrP2PPeerRole = errors.New("voice: invalid P2P peer role")

// P2PPeer is a direct two-player WebRTC data-channel endpoint.
// The authenticated Cinnabar broker, not this type, selects and authorizes the peer.
type P2PPeer struct {
	pc *webrtc.PeerConnection
	channel *webrtc.DataChannel
	sourceID uuid.UUID
	incoming chan []byte
	done chan struct{}
	ready chan struct{}
	mu sync.Mutex
	closed bool
	readyOnce sync.Once
	closeOnce sync.Once
	transmitAllowed atomic.Bool
	lastSequence atomic.Uint64
	droppedFrames atomic.Uint64
}

func NewP2PPeer(iceServers []webrtc.ICEServer, offerer bool) (*P2PPeer, error) {
	pc, err := webrtc.NewPeerConnection(webrtc.Configuration{ICEServers: append([]webrtc.ICEServer(nil), iceServers...)})
	if err != nil { return nil, err }
	peer := &P2PPeer{pc: pc, sourceID: uuid.New(), incoming: make(chan []byte, 32), done: make(chan struct{}), ready: make(chan struct{})}
	fail := func(err error) (*P2PPeer, error) { peer.Close(); return nil, err }
	pc.OnConnectionStateChange(func(state webrtc.PeerConnectionState) {
		if state == webrtc.PeerConnectionStateFailed || state == webrtc.PeerConnectionStateClosed { go peer.Close() }
	})
	if offerer {
		unordered, retransmits := false, uint16(0)
		channel, err := pc.CreateDataChannel(p2pDataChannelLabel, &webrtc.DataChannelInit{Ordered: &unordered, MaxRetransmits: &retransmits})
		if err != nil { return fail(err) }
		peer.attach(channel)
	} else {
		pc.OnDataChannel(func(channel *webrtc.DataChannel) {
			if channel.Label() != p2pDataChannelLabel { _ = channel.Close(); return }
			peer.attach(channel)
		})
	}
	return peer, nil
}

func (peer *P2PPeer) attach(channel *webrtc.DataChannel) {
	peer.mu.Lock()
	if peer.closed || peer.channel != nil { peer.mu.Unlock(); _ = channel.Close(); return }
	peer.channel = channel
	peer.mu.Unlock()
	channel.OnOpen(func(){ peer.readyOnce.Do(func(){ close(peer.ready) }) })
	channel.OnClose(func(){ go peer.Close() })
	channel.OnMessage(func(message webrtc.DataChannelMessage) {
		if message.IsString || len(message.Data) != voiceRemoteFrameBytes { peer.droppedFrames.Add(1); return }
		frame := message.Data[voiceSourceIDBytes:]
		sequence, ok := voiceFrameSequence(frame, peer.lastSequence.Load())
		if !ok || !peer.acceptSequence(sequence) { peer.droppedFrames.Add(1); return }
		packet := append([]byte(nil), message.Data...)
		select { case peer.incoming <- packet: case <-peer.done: default: peer.droppedFrames.Add(1) }
	})
}

func (peer *P2PPeer) acceptSequence(sequence uint64) bool {
	for { last := peer.lastSequence.Load(); if sequence <= last { return false }; if peer.lastSequence.CompareAndSwap(last, sequence) { return true } }
}

func (peer *P2PPeer) CreateOffer(ctx context.Context) (webrtc.SessionDescription, error) {
	offer, err := peer.pc.CreateOffer(nil); if err != nil { return webrtc.SessionDescription{}, err }
	gathered := webrtc.GatheringCompletePromise(peer.pc)
	if err = peer.pc.SetLocalDescription(offer); err != nil { return webrtc.SessionDescription{}, err }
	select { case <-ctx.Done(): return webrtc.SessionDescription{}, ctx.Err(); case <-peer.done: return webrtc.SessionDescription{}, ErrVoiceClientClosed; case <-time.After(30*time.Second): return webrtc.SessionDescription{}, errors.New("voice: P2P ICE gathering timed out"); case <-gathered: }
	return *peer.pc.LocalDescription(), nil
}

func (peer *P2PPeer) AcceptOffer(ctx context.Context, offer webrtc.SessionDescription) (webrtc.SessionDescription, error) {
	if offer.Type != webrtc.SDPTypeOffer || offer.SDP == "" { return webrtc.SessionDescription{}, ErrP2PPeerRole }
	if err := peer.pc.SetRemoteDescription(offer); err != nil { return webrtc.SessionDescription{}, err }
	answer, err := peer.pc.CreateAnswer(nil); if err != nil { return webrtc.SessionDescription{}, err }
	gathered := webrtc.GatheringCompletePromise(peer.pc)
	if err = peer.pc.SetLocalDescription(answer); err != nil { return webrtc.SessionDescription{}, err }
	select { case <-ctx.Done(): return webrtc.SessionDescription{}, ctx.Err(); case <-peer.done: return webrtc.SessionDescription{}, ErrVoiceClientClosed; case <-time.After(30*time.Second): return webrtc.SessionDescription{}, errors.New("voice: P2P ICE gathering timed out"); case <-gathered: }
	return *peer.pc.LocalDescription(), nil
}

func (peer *P2PPeer) AcceptAnswer(answer webrtc.SessionDescription) error {
	if answer.Type != webrtc.SDPTypeAnswer || answer.SDP == "" { return ErrP2PPeerRole }
	return peer.pc.SetRemoteDescription(answer)
}

func (peer *P2PPeer) WaitReady(ctx context.Context) error {
	select { case <-ctx.Done(): return ctx.Err(); case <-peer.done: return ErrVoiceClientClosed; case <-peer.ready: return nil }
}

func (peer *P2PPeer) SetTransmitAllowed(allowed bool) { peer.transmitAllowed.Store(allowed) }

func (peer *P2PPeer) SendFrame(frame []byte) error {
	if !peer.transmitAllowed.Load() { return ErrVoiceTransmitDisabled }
	if len(frame) != voiceFrameBytes { return ErrVoiceFrameInvalid }
	select { case <-peer.done: return ErrVoiceClientClosed; default: }
	peer.mu.Lock(); channel, closed := peer.channel, peer.closed; peer.mu.Unlock()
	if closed || channel == nil || channel.ReadyState() != webrtc.DataChannelStateOpen { return ErrP2PNotReady }
	if channel.BufferedAmount() > maxBufferedVoiceBytes { peer.droppedFrames.Add(1); return ErrVoiceBackpressure }
	packet := make([]byte, voiceRemoteFrameBytes)
	copy(packet[:voiceSourceIDBytes], peer.sourceID[:]); copy(packet[voiceSourceIDBytes:], frame)
	if err := channel.Send(packet); err != nil { peer.droppedFrames.Add(1); return err }
	return nil
}

func (peer *P2PPeer) ReceiveFrames() <-chan []byte { return peer.incoming }
func (peer *P2PPeer) Done() <-chan struct{} { return peer.done }
func (peer *P2PPeer) DroppedFrames() uint64 { return peer.droppedFrames.Load() }

func (peer *P2PPeer) Close() {
	if peer == nil { return }
	peer.closeOnce.Do(func() {
		peer.mu.Lock(); peer.closed = true; channel, pc := peer.channel, peer.pc; peer.mu.Unlock()
		peer.transmitAllowed.Store(false); close(peer.done)
		if channel != nil { _ = channel.Close() }; if pc != nil { _ = pc.Close() }
	})
}
