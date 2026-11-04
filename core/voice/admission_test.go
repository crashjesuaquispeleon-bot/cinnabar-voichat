package voice

import (
	"crypto/ed25519"
	"crypto/rand"
	"encoding/base64"
	"encoding/json"
	"errors"
	"testing"
	"time"

	"github.com/google/uuid"
)

func signedToken(t *testing.T, key ed25519.PrivateKey, claims jwtClaims) string {
	t.Helper()
	header, err := json.Marshal(jwtHeader{Algorithm: "EdDSA", Type: "JWT"})
	if err != nil { t.Fatal(err) }
	payload, err := json.Marshal(claims)
	if err != nil { t.Fatal(err) }
	input := base64.RawURLEncoding.EncodeToString(header) + "." + base64.RawURLEncoding.EncodeToString(payload)
	return input + "." + base64.RawURLEncoding.EncodeToString(ed25519.Sign(key, []byte(input)))
}

func verifier(t *testing.T) (ed25519.PrivateKey, VerifyConfig) {
	t.Helper()
	public, private, err := ed25519.GenerateKey(rand.Reader)
	if err != nil { t.Fatal(err) }
	return private, VerifyConfig{Issuer: "https://api.cinnabar.gg", Audience: "cinnabar-voice-sfu", PublicKey: public, MaxTTL: 2*time.Minute}
}

func claimsAt(now time.Time) jwtClaims {
	return jwtClaims{
		Issuer: "https://api.cinnabar.gg", Audience: "cinnabar-voice-sfu",
		Subject: "account-123", XUID: "2535467890123456",
		RoomID: uuid.NewString(), SessionID: uuid.NewString(), TokenID: uuid.NewString(),
		IssuedAt: now.Unix(), NotBefore: now.Unix(), ExpiresAt: now.Add(time.Minute).Unix(),
	}
}

func admissionFor(t *testing.T, now time.Time, roomID, sessionID, xuid string) Admission {
	t.Helper()
	key, config := verifier(t)
	claims := claimsAt(now)
	claims.RoomID, claims.SessionID, claims.XUID = roomID, sessionID, xuid
	a, err := VerifyAdmission(signedToken(t, key, claims), config, now)
	if err != nil { t.Fatal(err) }
	return a
}

func TestVerifyAdmissionValidatesSignatureIssuerAudienceAndTime(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	key, config := verifier(t)
	claims := claimsAt(now)
	token := signedToken(t, key, claims)
	a, err := VerifyAdmission(token, config, now)
	if err != nil { t.Fatal(err) }
	if a.Subject != claims.Subject || a.XUID != claims.XUID || a.RoomID != claims.RoomID || a.SessionID != claims.SessionID || !a.verified {
		t.Fatalf("verified claims not preserved: %#v", a)
	}
	if _, err := VerifyAdmission(token+"x", config, now); !errors.Is(err, ErrInvalidAdmission) {
		t.Fatalf("tampered token error = %v", err)
	}
	wrongAudience := config
	wrongAudience.Audience = "another-service"
	if _, err := VerifyAdmission(token, wrongAudience, now); !errors.Is(err, ErrInvalidAdmission) {
		t.Fatalf("wrong audience error = %v", err)
	}
	wrongIssuer := config
	wrongIssuer.Issuer = "https://other.example"
	if _, err := VerifyAdmission(token, wrongIssuer, now); !errors.Is(err, ErrInvalidAdmission) {
		t.Fatalf("wrong issuer error = %v", err)
	}
}

func TestVerifyAdmissionRejectsExpiredLongLivedAndMalformedClaims(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	key, config := verifier(t)
	cases := []struct{name string; edit func(*jwtClaims); want error}{
		{"expired", func(c *jwtClaims){ c.IssuedAt = now.Add(-2*time.Minute).Unix(); c.NotBefore = c.IssuedAt; c.ExpiresAt = now.Add(-time.Minute).Unix() }, ErrAdmissionExpired},
		{"long lifetime", func(c *jwtClaims){ c.ExpiresAt = now.Add(6*time.Minute).Unix() }, ErrInvalidAdmission},
		{"invalid xuid", func(c *jwtClaims){ c.XUID = "not-an-xuid" }, ErrInvalidAdmission},
		{"invalid room", func(c *jwtClaims){ c.RoomID = "not-a-uuid" }, ErrInvalidAdmission},
		{"future nbf", func(c *jwtClaims){ c.NotBefore = now.Add(time.Minute).Unix() }, ErrAdmissionNotYetValid},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			claims := claimsAt(now)
			test.edit(&claims)
			_, err := VerifyAdmission(signedToken(t, key, claims), config, now)
			if !errors.Is(err, test.want) { t.Fatalf("error = %v, want %v", err, test.want) }
		})
	}
}

func TestRegistryBindsOneRoomToOneSessionAndCapsMembers(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	r, err := NewRegistry(2)
	if err != nil { t.Fatal(err) }
	roomID, sessionID := uuid.NewString(), uuid.NewString()
	first := admissionFor(t, now, roomID, sessionID, "1001")
	second := admissionFor(t, now, roomID, sessionID, "1002")
	if err := r.Join(first, now); err != nil { t.Fatal(err) }
	if err := r.Join(second, now); err != nil { t.Fatal(err) }
	if len(r.Participants(roomID)) != 2 { t.Fatal("both participants should be present") }
	if err := r.Join(admissionFor(t, now, roomID, uuid.NewString(), "1003"), now); !errors.Is(err, ErrRoomSessionMismatch) {
		t.Fatalf("session mismatch error = %v", err)
	}
	if err := r.Join(admissionFor(t, now, roomID, sessionID, "1003"), now); !errors.Is(err, ErrRoomFull) {
		t.Fatalf("capacity error = %v", err)
	}
	if err := r.Join(Admission{RoomID: roomID, SessionID: sessionID, XUID: "bad"}, now); !errors.Is(err, ErrAdmissionNotVerified) {
		t.Fatalf("unverified admission error = %v", err)
	}
}

func TestRegistryRenewsLeaseAndRejectsTokenReplay(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	r, err := NewRegistry(8)
	if err != nil { t.Fatal(err) }
	roomID, sessionID := uuid.NewString(), uuid.NewString()
	first := admissionFor(t, now, roomID, sessionID, "2001")
	if err := r.Join(first, now); err != nil { t.Fatal(err) }
	renewed := admissionFor(t, now.Add(30*time.Second), roomID, sessionID, "2001")
	if err := r.Renew(renewed, now.Add(30*time.Second)); err != nil { t.Fatal(err) }
	if err := r.Join(first, now.Add(30*time.Second)); !errors.Is(err, ErrAdmissionAlreadyUsed) {
		t.Fatalf("replayed token error = %v", err)
	}
	if got := r.Expire(now.Add(61*time.Second)); len(got) != 0 {
		t.Fatalf("renewed member expired with old lease: %#v", got)
	}
	if got := r.Expire(now.Add(91*time.Second)); len(got) != 1 || got[0].TokenID != renewed.TokenID {
		t.Fatalf("renewal expiry = %#v", got)
	}
}

func TestRegistryRevokesExpiresAndClosesOnlyMatchingSession(t *testing.T) {
	now := time.Unix(1_800_000_000, 0)
	r, err := NewRegistry(8)
	if err != nil { t.Fatal(err) }
	roomID, sessionID := uuid.NewString(), uuid.NewString()
	first := admissionFor(t, now, roomID, sessionID, "2001")
	second := admissionFor(t, now, roomID, sessionID, "2002")
	if err := r.Join(first, now); err != nil { t.Fatal(err) }
	if err := r.Join(second, now); err != nil { t.Fatal(err) }
	if !r.Revoke(first.TokenID) || r.Revoke(first.TokenID) { t.Fatal("revoke must be idempotent") }
	if got := r.CloseSession(roomID, uuid.NewString()); len(got) != 0 { t.Fatal("wrong session closed a room") }
	if got := r.CloseSession(roomID, sessionID); len(got) != 1 || got[0].XUID != second.XUID { t.Fatalf("closed participants = %#v", got) }
	expiring := admissionFor(t, now, roomID, sessionID, "2003")
	if err := r.Join(expiring, now); err != nil { t.Fatal(err) }
	if got := r.Expire(now.Add(2*time.Minute)); len(got) != 1 || got[0].XUID != expiring.XUID { t.Fatalf("expired = %#v", got) }
	if r.Revoke(expiring.TokenID) { t.Fatal("expired token remained active") }
}

func TestRegistryRejectsInvalidParticipantLimits(t *testing.T) {
	if _, err := NewRegistry(0); !errors.Is(err, ErrInvalidRegistryLimit) { t.Fatal("zero limit accepted") }
	if _, err := NewRegistry(maxRoomParticipants+1); !errors.Is(err, ErrInvalidRegistryLimit) { t.Fatal("oversized limit accepted") }
}
