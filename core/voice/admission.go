package voice

import (
	"crypto/ed25519"
	"encoding/base64"
	"encoding/json"
	"errors"
	"io"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/google/uuid"
)

const (
	maxAdmissionTokenBytes = 4096
	defaultAdmissionTTL = 2 * time.Minute
	maxAdmissionTTL = 5 * time.Minute
	clockSkew = 15 * time.Second
	maxSubjectBytes = 128
	maxRoomParticipants = 64
)

var (
	ErrInvalidAdmission = errors.New("voice: invalid admission")
	ErrAdmissionExpired = errors.New("voice: admission expired")
	ErrAdmissionNotYetValid = errors.New("voice: admission not yet valid")
	ErrRoomSessionMismatch = errors.New("voice: room is bound to another game session")
	ErrRoomFull = errors.New("voice: room is full")
	ErrParticipantNotPresent = errors.New("voice: participant is not present")
	ErrParticipantPresent = errors.New("voice: participant already present")
	ErrAdmissionAlreadyUsed = errors.New("voice: admission token already used")
	ErrAdmissionNotVerified = errors.New("voice: admission was not verified")
	ErrInvalidRegistryLimit = errors.New("voice: invalid room participant limit")
)

// VerifyConfig pins the issuer, audience and Ed25519 public key trusted by the SFU.
// The signing key must remain in the Cinnabar broker.
type VerifyConfig struct {
	Issuer string
	Audience string
	PublicKey ed25519.PublicKey
	MaxTTL time.Duration
}

type jwtHeader struct {
	Algorithm string `json:"alg"`
	Type string `json:"typ"`
}

// jwtClaims are signed only after the broker verifies account identity and authoritative
// Minecraft partida membership. RoomID and SessionID are never client-selected.
type jwtClaims struct {
	Issuer string `json:"iss"`
	Audience string `json:"aud"`
	Subject string `json:"sub"`
	XUID string `json:"xuid"`
	RoomID string `json:"room_id"`
	SessionID string `json:"session_id"`
	TokenID string `json:"jti"`
	IssuedAt int64 `json:"iat"`
	NotBefore int64 `json:"nbf"`
	ExpiresAt int64 `json:"exp"`
}

// Admission contains verified, short-lived permission for one exact game session.
// Registry refuses values not returned by VerifyAdmission. The bearer token is not retained.
type Admission struct {
	Subject string
	XUID string
	RoomID string
	SessionID string
	TokenID string
	IssuedAt time.Time
	NotBefore time.Time
	ExpiresAt time.Time
	verified bool
}

// VerifyAdmission checks a compact EdDSA JWT. The broker derives XUID from the authenticated
// account and SessionID from a trusted server-side partida adapter.
func VerifyAdmission(token string, config VerifyConfig, now time.Time) (Admission, error) {
	if len(token) == 0 || len(token) > maxAdmissionTokenBytes ||
		config.Issuer == "" || config.Audience == "" ||
		len(config.PublicKey) != ed25519.PublicKeySize {
		return Admission{}, ErrInvalidAdmission
	}
	if config.MaxTTL <= 0 {
		config.MaxTTL = defaultAdmissionTTL
	}
	if config.MaxTTL > maxAdmissionTTL {
		return Admission{}, ErrInvalidAdmission
	}
	parts := strings.Split(token, ".")
	if len(parts) != 3 {
		return Admission{}, ErrInvalidAdmission
	}
	headerBytes, err := base64.RawURLEncoding.Strict().DecodeString(parts[0])
	if err != nil {
		return Admission{}, ErrInvalidAdmission
	}
	var header jwtHeader
	if decodeOneJSON(headerBytes, &header) != nil || header.Algorithm != "EdDSA" || header.Type != "JWT" {
		return Admission{}, ErrInvalidAdmission
	}
	signature, err := base64.RawURLEncoding.Strict().DecodeString(parts[2])
	if err != nil || len(signature) != ed25519.SignatureSize ||
		!ed25519.Verify(config.PublicKey, []byte(parts[0]+"."+parts[1]), signature) {
		return Admission{}, ErrInvalidAdmission
	}
	payload, err := base64.RawURLEncoding.Strict().DecodeString(parts[1])
	if err != nil {
		return Admission{}, ErrInvalidAdmission
	}
	var claims jwtClaims
	if decodeOneJSON(payload, &claims) != nil ||
		claims.Issuer != config.Issuer || claims.Audience != config.Audience ||
		!validSubject(claims.Subject) || !validXUID(claims.XUID) ||
		!canonicalUUID(claims.RoomID) || !canonicalUUID(claims.SessionID) || !canonicalUUID(claims.TokenID) {
		return Admission{}, ErrInvalidAdmission
	}
	maxTTLSeconds := int64(config.MaxTTL / time.Second)
	if claims.IssuedAt <= 0 || claims.NotBefore <= 0 || claims.ExpiresAt <= claims.IssuedAt ||
		claims.ExpiresAt-claims.IssuedAt > maxTTLSeconds {
		return Admission{}, ErrInvalidAdmission
	}
	issued, notBefore, expires := time.Unix(claims.IssuedAt, 0), time.Unix(claims.NotBefore, 0), time.Unix(claims.ExpiresAt, 0)
	if issued.After(now.Add(clockSkew)) {
		return Admission{}, ErrInvalidAdmission
	}
	if notBefore.After(now.Add(clockSkew)) {
		return Admission{}, ErrAdmissionNotYetValid
	}
	if !expires.After(now) {
		return Admission{}, ErrAdmissionExpired
	}
	return Admission{
		Subject: claims.Subject, XUID: claims.XUID, RoomID: claims.RoomID, SessionID: claims.SessionID,
		TokenID: claims.TokenID, IssuedAt: issued, NotBefore: notBefore, ExpiresAt: expires, verified: true,
	}, nil
}

func decodeOneJSON(data []byte, target any) error {
	decoder := json.NewDecoder(strings.NewReader(string(data)))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(target); err != nil {
		return err
	}
	if err := decoder.Decode(new(any)); !errors.Is(err, io.EOF) {
		if err == nil {
			return errors.New("voice: trailing JSON value")
		}
		return err
	}
	return nil
}

func validSubject(subject string) bool {
	return subject != "" && len(subject) <= maxSubjectBytes && utf8.ValidString(subject) && strings.TrimSpace(subject) == subject
}

func validXUID(xuid string) bool {
	if xuid == "" || len(xuid) > 20 {
		return false
	}
	for _, char := range xuid {
		if char < '0' || char > '9' {
			return false
		}
	}
	return true
}

func canonicalUUID(value string) bool {
	id, err := uuid.Parse(value)
	return err == nil && id.String() == value
}

// Participant is a safe snapshot without bearer tokens or device metadata.
type Participant struct {
	RoomID string
	TokenID string
	Subject string
	XUID string
	ExpiresAt time.Time
}

type roomState struct {
	sessionID string
	members map[string]Admission
}

// Registry enforces room/session isolation and participant limits in one SFU process.
// The SFU must call Revoke on peer close or broker revocation, and Expire periodically.
type Registry struct {
	mu sync.Mutex
	maxMembers int
	rooms map[string]*roomState
	tokenToRoom map[string]string
	usedTokens map[string]time.Time
}

func NewRegistry(maxMembers int) (*Registry, error) {
	if maxMembers < 1 || maxMembers > maxRoomParticipants {
		return nil, ErrInvalidRegistryLimit
	}
	return &Registry{maxMembers: maxMembers, rooms: make(map[string]*roomState), tokenToRoom: make(map[string]string), usedTokens: make(map[string]time.Time)}, nil
}

// Join adds a verified admission. A room UUID cannot span multiple game-session UUIDs.
func (r *Registry) Join(a Admission, now time.Time) error {
	if !a.verified {
		return ErrAdmissionNotVerified
	}
	if !a.ExpiresAt.After(now) || a.TokenID == "" {
		return ErrAdmissionExpired
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	r.expireLocked(now)
	if _, exists := r.usedTokens[a.TokenID]; exists {
		return ErrAdmissionAlreadyUsed
	}
	room := r.rooms[a.RoomID]
	if room == nil {
		room = &roomState{sessionID: a.SessionID, members: make(map[string]Admission)}
		r.rooms[a.RoomID] = room
	} else if room.sessionID != a.SessionID {
		return ErrRoomSessionMismatch
	}
	if _, exists := room.members[a.XUID]; exists {
		return ErrParticipantPresent
	}
	if len(room.members) >= r.maxMembers {
		return ErrRoomFull
	}
	room.members[a.XUID] = a
	r.tokenToRoom[a.TokenID] = a.RoomID
	r.usedTokens[a.TokenID] = a.ExpiresAt
	return nil
}

// Renew replaces an active participant's lease without replacing their media connection.
// A renewal must be freshly signed for the same account, XUID, room and game session.
func (r *Registry) Renew(a Admission, now time.Time) error {
	if !a.verified {
		return ErrAdmissionNotVerified
	}
	if !a.ExpiresAt.After(now) || a.TokenID == "" {
		return ErrAdmissionExpired
	}
	r.mu.Lock()
	defer r.mu.Unlock()
	r.expireLocked(now)
	if _, exists := r.usedTokens[a.TokenID]; exists {
		return ErrAdmissionAlreadyUsed
	}
	room := r.rooms[a.RoomID]
	if room == nil || room.sessionID != a.SessionID {
		return ErrRoomSessionMismatch
	}
	current, exists := room.members[a.XUID]
	if !exists || current.Subject != a.Subject {
		return ErrParticipantNotPresent
	}
	delete(r.tokenToRoom, current.TokenID)
	room.members[a.XUID] = a
	r.tokenToRoom[a.TokenID] = a.RoomID
	r.usedTokens[a.TokenID] = a.ExpiresAt
	return nil
}

// Revoke removes the participant named by one admission token. Repeated calls are harmless.
func (r *Registry) Revoke(tokenID string) bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	roomID, exists := r.tokenToRoom[tokenID]
	if !exists {
		return false
	}
	delete(r.tokenToRoom, tokenID)
	room := r.rooms[roomID]
	if room == nil {
		return false
	}
	for xuid, admission := range room.members {
		if admission.TokenID == tokenID {
			delete(room.members, xuid)
			break
		}
	}
	if len(room.members) == 0 {
		delete(r.rooms, roomID)
	}
	return true
}

func (r *Registry) Expire(now time.Time) []Participant {
	r.mu.Lock()
	defer r.mu.Unlock()
	return r.expireLocked(now)
}

func (r *Registry) expireLocked(now time.Time) []Participant {
	var expired []Participant
	for roomID, room := range r.rooms {
		for xuid, admission := range room.members {
			if !admission.ExpiresAt.After(now) {
				expired = append(expired, Participant{RoomID: roomID, TokenID: admission.TokenID, Subject: admission.Subject, XUID: xuid, ExpiresAt: admission.ExpiresAt})
				delete(room.members, xuid)
				delete(r.tokenToRoom, admission.TokenID)
			}
		}
		if len(room.members) == 0 {
			delete(r.rooms, roomID)
		}
	}
	for tokenID, expires := range r.usedTokens {
		if !expires.After(now) {
			delete(r.usedTokens, tokenID)
		}
	}
	return expired
}

func (r *Registry) Participants(roomID string) []Participant {
	r.mu.Lock()
	defer r.mu.Unlock()
	room := r.rooms[roomID]
	if room == nil {
		return nil
	}
	out := make([]Participant, 0, len(room.members))
	for xuid, admission := range room.members {
		out = append(out, Participant{RoomID: roomID, TokenID: admission.TokenID, Subject: admission.Subject, XUID: xuid, ExpiresAt: admission.ExpiresAt})
	}
	return out
}

// CloseSession revokes all participants only when the supplied session binding matches.
func (r *Registry) CloseSession(roomID, sessionID string) []Participant {
	r.mu.Lock()
	defer r.mu.Unlock()
	room := r.rooms[roomID]
	if room == nil || room.sessionID != sessionID {
		return nil
	}
	out := make([]Participant, 0, len(room.members))
	for xuid, admission := range room.members {
		out = append(out, Participant{RoomID: roomID, TokenID: admission.TokenID, Subject: admission.Subject, XUID: xuid, ExpiresAt: admission.ExpiresAt})
		delete(r.tokenToRoom, admission.TokenID)
	}
	delete(r.rooms, roomID)
	return out
}

func (r *Registry) RoomCount() int {
	r.mu.Lock()
	defer r.mu.Unlock()
	return len(r.rooms)
}
