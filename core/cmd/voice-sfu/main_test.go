package main

import (
	"crypto/ed25519"
	"encoding/base64"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"

	"github.com/google/uuid"
)

func validVoiceSFUEnvironment() map[string]string {
	publicKey := make(ed25519.PublicKey, ed25519.PublicKeySize)
	return map[string]string{
		"CINNABAR_VOICE_LISTEN_ADDR":      "127.0.0.1:9443",
		"CINNABAR_VOICE_TLS_CERT_FILE":    "/run/secrets/voice.crt",
		"CINNABAR_VOICE_TLS_KEY_FILE":     "/run/secrets/voice.key",
		"CINNABAR_VOICE_ISSUER":           "https://api.cinnabar.gg/voice",
		"CINNABAR_VOICE_BROKER_PUBLIC_KEY": base64.StdEncoding.EncodeToString(publicKey),
		"CINNABAR_VOICE_CONTROL_TOKEN": base64.StdEncoding.EncodeToString(make([]byte, 32)),
		"CINNABAR_VOICE_ICE_SERVERS":      `[{"urls":["turns:turn.cinnabar.gg:5349"],"username":"ephemeral","credential":"secret"}]`,
	}
}

func TestLoadConfigUsesPinnedBrokerKeyAndExplicitICE(t *testing.T) {
	values := validVoiceSFUEnvironment()
	cfg, err := loadConfig(func(key string) string { return values[key] })
	if err != nil {
		t.Fatal(err)
	}
	if cfg.ListenAddress != "127.0.0.1:9443" || cfg.Audience != defaultAudience {
		t.Fatalf("unexpected listener/audience config: %#v", cfg)
	}
	if len(cfg.PublicKey) != ed25519.PublicKeySize || len(cfg.ControlToken) != 32 ||
		len(cfg.ICEServers) != 1 || cfg.MaxParticipants != defaultRoomLimit {
		t.Fatalf("unexpected SFU security config: %#v", cfg)
	}
}

func TestLoadConfigRejectsMissingTLSIssuerKeyAndICE(t *testing.T) {
	cases := []struct {
		name string
		edit func(map[string]string)
	}{
		{name: "missing TLS key", edit: func(values map[string]string) {
			delete(values, "CINNABAR_VOICE_TLS_KEY_FILE")
		}},
		{name: "non HTTPS issuer", edit: func(values map[string]string) {
			values["CINNABAR_VOICE_ISSUER"] = "http://api.cinnabar.gg"
		}},
		{name: "wrong public key size", edit: func(values map[string]string) {
			values["CINNABAR_VOICE_BROKER_PUBLIC_KEY"] = base64.StdEncoding.EncodeToString([]byte("too short"))
		}},
		{name: "missing control token", edit: func(values map[string]string) {
			delete(values, "CINNABAR_VOICE_CONTROL_TOKEN")
		}},
		{name: "short control token", edit: func(values map[string]string) {
			values["CINNABAR_VOICE_CONTROL_TOKEN"] = base64.StdEncoding.EncodeToString([]byte("short"))
		}},
		{name: "missing ICE", edit: func(values map[string]string) {
			delete(values, "CINNABAR_VOICE_ICE_SERVERS")
		}},
		{name: "invalid participant cap", edit: func(values map[string]string) {
			values["CINNABAR_VOICE_MAX_PARTICIPANTS"] = "65"
		}},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			values := validVoiceSFUEnvironment()
			test.edit(values)
			if _, err := loadConfig(func(key string) string { return values[key] }); err == nil {
				t.Fatal("invalid voice SFU configuration was accepted")
			}
		})
	}
}

func TestLoadConfigRejectsEmptyICEURLs(t *testing.T) {
	values := validVoiceSFUEnvironment()
	values["CINNABAR_VOICE_ICE_SERVERS"] = `[{"urls":[]}]`
	if _, err := loadConfig(func(key string) string { return values[key] }); err == nil {
		t.Fatal("ICE server without URLs was accepted")
	}
}

func TestDecodePublicKeyAcceptsBase64AndRejectsMalformedInput(t *testing.T) {
	key := make(ed25519.PublicKey, ed25519.PublicKeySize)
	for _, encoded := range []string{
		base64.StdEncoding.EncodeToString(key),
		base64.RawStdEncoding.EncodeToString(key),
		base64.RawURLEncoding.EncodeToString(key),
	} {
		decoded, err := decodePublicKey(encoded)
		if err != nil || len(decoded) != ed25519.PublicKeySize {
			t.Fatalf("decode public key: length=%d err=%v", len(decoded), err)
		}
	}
	if _, err := decodePublicKey("not-base64"); err == nil {
		t.Fatal("malformed key was accepted")
	}
}

func TestICEConfigRoundTripsWithoutLoggingSecrets(t *testing.T) {
	values := validVoiceSFUEnvironment()
	cfg, err := loadConfig(func(key string) string { return values[key] })
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(cfg.ICEServers)
	if err != nil {
		t.Fatal(err)
	}
	if string(encoded) == "" {
		t.Fatal("ICE configuration unexpectedly empty")
	}
}


func TestDecodeControlTokenRequiresExactlyThirtyTwoBytes(t *testing.T) {
	token := make([]byte, 32)
	for _, encoded := range []string{
		base64.StdEncoding.EncodeToString(token),
		base64.RawStdEncoding.EncodeToString(token),
		base64.RawURLEncoding.EncodeToString(token),
	} {
		decoded, err := decodeControlToken(encoded)
		if err != nil || len(decoded) != 32 {
			t.Fatalf("decode control token: length=%d err=%v", len(decoded), err)
		}
	}
	if _, err := decodeControlToken(base64.StdEncoding.EncodeToString(make([]byte, 31))); err == nil {
		t.Fatal("short control token was accepted")
	}
}

func TestSessionCloseHandlerRequiresBrokerControlTokenAndCanonicalIDs(t *testing.T) {
	token := []byte("0123456789abcdef0123456789abcdef")
	roomID, sessionID := uuid.NewString(), uuid.NewString()
	closed := 0
	mux := http.NewServeMux()
	mux.Handle("POST /v1/admin/rooms/{roomID}/sessions/{sessionID}/close",
		sessionCloseHandler(token, func(room, session string) int {
			if room != roomID || session != sessionID {
				t.Fatalf("unexpected close binding room=%q session=%q", room, session)
			}
			closed++
			return 2
		}))
	path := "/v1/admin/rooms/" + roomID + "/sessions/" + sessionID + "/close"

	request := httptest.NewRequest(http.MethodPost, path, nil)
	response := httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized || closed != 0 {
		t.Fatalf("missing token response=%d closed=%d", response.Code, closed)
	}

	request = httptest.NewRequest(http.MethodPost, path, nil)
	request.Header.Set("Authorization", "Bearer "+"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized || closed != 0 {
		t.Fatalf("wrong token response=%d closed=%d", response.Code, closed)
	}

	request = httptest.NewRequest(http.MethodPost, "/v1/admin/rooms/not-a-uuid/sessions/"+sessionID+"/close", nil)
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest || closed != 0 {
		t.Fatalf("invalid room response=%d closed=%d", response.Code, closed)
	}

	request = httptest.NewRequest(http.MethodPost, path, strings.NewReader("unexpected"))
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest || closed != 0 {
		t.Fatalf("body response=%d closed=%d", response.Code, closed)
	}

	request = httptest.NewRequest(http.MethodPost, path, strings.NewReader("chunked body"))
	request.ContentLength = -1
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest || closed != 0 {
		t.Fatalf("unknown-length body response=%d closed=%d", response.Code, closed)
	}

	request = httptest.NewRequest(http.MethodPost, path, nil)
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusNoContent || closed != 1 {
		t.Fatalf("valid close response=%d closed=%d", response.Code, closed)
	}
	if response.Header().Get("Cache-Control") != "no-store" {
		t.Fatal("admin response must not be cached")
	}
}

func TestAdmissionRevokeHandlerRequiresBrokerControlTokenAndCanonicalIDs(t *testing.T) {
	token := []byte("0123456789abcdef0123456789abcdef")
	roomID, tokenID := uuid.NewString(), uuid.NewString()
	revoked := 0
	mux := http.NewServeMux()
	mux.Handle("POST /v1/admin/rooms/{roomID}/admissions/{tokenID}/revoke",
		admissionRevokeHandler(token, func(room, admission string) bool {
			if room != roomID || admission != tokenID {
				t.Fatalf("unexpected revoke binding room=%q token=%q", room, admission)
			}
			revoked++
			return true
		}))
	path := "/v1/admin/rooms/" + roomID + "/admissions/" + tokenID + "/revoke"

	request := httptest.NewRequest(http.MethodPost, path, nil)
	response := httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized || revoked != 0 {
		t.Fatalf("missing token response=%d revoked=%d", response.Code, revoked)
	}

	request = httptest.NewRequest(http.MethodPost, "/v1/admin/rooms/not-a-uuid/admissions/"+tokenID+"/revoke", nil)
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest || revoked != 0 {
		t.Fatalf("invalid room response=%d revoked=%d", response.Code, revoked)
	}

	request = httptest.NewRequest(http.MethodPost, "/v1/admin/rooms/"+roomID+"/admissions/not-a-uuid/revoke", nil)
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest || revoked != 0 {
		t.Fatalf("invalid token ID response=%d revoked=%d", response.Code, revoked)
	}

	request = httptest.NewRequest(http.MethodPost, path, strings.NewReader("unexpected"))
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusBadRequest || revoked != 0 {
		t.Fatalf("body response=%d revoked=%d", response.Code, revoked)
	}

	request = httptest.NewRequest(http.MethodPost, path, nil)
	request.Header.Set("Authorization", "Bearer "+string(token))
	response = httptest.NewRecorder()
	mux.ServeHTTP(response, request)
	if response.Code != http.StatusNoContent || revoked != 1 {
		t.Fatalf("valid revoke response=%d revoked=%d", response.Code, revoked)
	}
	if response.Header().Get("Cache-Control") != "no-store" {
		t.Fatal("admin response must not be cached")
	}
}
