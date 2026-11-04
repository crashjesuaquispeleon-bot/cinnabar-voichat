package main

import (
	"crypto/ed25519"
	"encoding/base64"
	"encoding/json"
	"testing"
)

func validVoiceSFUEnvironment() map[string]string {
	publicKey := make(ed25519.PublicKey, ed25519.PublicKeySize)
	return map[string]string{
		"CINNABAR_VOICE_LISTEN_ADDR":      "127.0.0.1:9443",
		"CINNABAR_VOICE_TLS_CERT_FILE":    "/run/secrets/voice.crt",
		"CINNABAR_VOICE_TLS_KEY_FILE":     "/run/secrets/voice.key",
		"CINNABAR_VOICE_ISSUER":           "https://api.cinnabar.gg/voice",
		"CINNABAR_VOICE_BROKER_PUBLIC_KEY": base64.StdEncoding.EncodeToString(publicKey),
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
	if len(cfg.PublicKey) != ed25519.PublicKeySize || len(cfg.ICEServers) != 1 ||
		cfg.MaxParticipants != defaultRoomLimit {
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
