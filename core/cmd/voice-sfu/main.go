package main

import (
	"context"
	"crypto/ed25519"
	"crypto/subtle"
	"crypto/tls"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"strconv"
	"strings"
	"syscall"
	"time"

	"github.com/google/uuid"
	"github.com/hashimthearab/rust-mcbe/core/voice"
	"github.com/pion/webrtc/v4"
)

const (
	defaultListenAddress = "127.0.0.1:8443"
	defaultAudience      = "cinnabar-voice-sfu"
	defaultRoomLimit     = 8
	maxRoomLimit         = 64
	shutdownTimeout      = 10 * time.Second
)

// config contains operator-owned SFU settings. It never contains the broker's signing key.
type config struct {
	ListenAddress string
	TLSCertFile   string
	TLSKeyFile    string
	Issuer        string
	Audience      string
	PublicKey     ed25519.PublicKey
	ControlToken  []byte
	ICEServers    []webrtc.ICEServer
	MaxParticipants int
}

func main() {
	cfg, err := loadConfig(os.Getenv)
	if err != nil {
		slog.Error("invalid voice SFU configuration", "error", err)
		os.Exit(2)
	}

	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	if err := run(ctx, cfg); err != nil && !errors.Is(err, context.Canceled) {
		slog.Error("voice SFU stopped", "error", err)
		os.Exit(1)
	}
}

func loadConfig(getenv func(string) string) (config, error) {
	if getenv == nil {
		return config{}, errors.New("voice SFU: environment reader is required")
	}

	cfg := config{
		ListenAddress: strings.TrimSpace(getenv("CINNABAR_VOICE_LISTEN_ADDR")),
		TLSCertFile:   strings.TrimSpace(getenv("CINNABAR_VOICE_TLS_CERT_FILE")),
		TLSKeyFile:    strings.TrimSpace(getenv("CINNABAR_VOICE_TLS_KEY_FILE")),
		Issuer:        strings.TrimSpace(getenv("CINNABAR_VOICE_ISSUER")),
		Audience:      strings.TrimSpace(getenv("CINNABAR_VOICE_AUDIENCE")),
		MaxParticipants: defaultRoomLimit,
	}
	if cfg.ListenAddress == "" {
		cfg.ListenAddress = defaultListenAddress
	}
	if cfg.Audience == "" {
		cfg.Audience = defaultAudience
	}
	if _, _, err := net.SplitHostPort(cfg.ListenAddress); err != nil {
		return config{}, fmt.Errorf("voice SFU: invalid listen address: %w", err)
	}
	if cfg.TLSCertFile == "" || cfg.TLSKeyFile == "" {
		return config{}, errors.New("voice SFU: TLS certificate and key file paths are required")
	}
	issuer, err := url.Parse(cfg.Issuer)
	if err != nil || issuer.Scheme != "https" || issuer.Host == "" ||
		issuer.User != nil || issuer.RawQuery != "" || issuer.Fragment != "" {
		return config{}, errors.New("voice SFU: issuer must be an HTTPS origin or path without credentials or query")
	}
	if cfg.Audience == "" {
		return config{}, errors.New("voice SFU: audience is required")
	}

	publicKeyText := strings.TrimSpace(getenv("CINNABAR_VOICE_BROKER_PUBLIC_KEY"))
	if publicKeyText == "" {
		return config{}, errors.New("voice SFU: broker Ed25519 public key is required")
	}
	publicKey, err := decodePublicKey(publicKeyText)
	if err != nil {
		return config{}, fmt.Errorf("voice SFU: invalid broker public key: %w", err)
	}
	cfg.PublicKey = publicKey

	controlText := strings.TrimSpace(getenv("CINNABAR_VOICE_CONTROL_TOKEN"))
	if controlText == "" {
		return config{}, errors.New("voice SFU: broker control token is required")
	}
	controlToken, err := decodeControlToken(controlText)
	if err != nil {
		return config{}, fmt.Errorf("voice SFU: invalid broker control token: %w", err)
	}
	cfg.ControlToken = controlToken

	iceText := strings.TrimSpace(getenv("CINNABAR_VOICE_ICE_SERVERS"))
	if iceText == "" {
		return config{}, errors.New("voice SFU: explicit STUN/TURN ICE server configuration is required")
	}
	if err := json.Unmarshal([]byte(iceText), &cfg.ICEServers); err != nil {
		return config{}, fmt.Errorf("voice SFU: invalid ICE server JSON: %w", err)
	}
	if len(cfg.ICEServers) == 0 {
		return config{}, errors.New("voice SFU: at least one ICE server is required")
	}
	for index, server := range cfg.ICEServers {
		if len(server.URLs) == 0 {
			return config{}, fmt.Errorf("voice SFU: ICE server %d has no URLs", index)
		}
		for _, iceURL := range server.URLs {
			if !strings.HasPrefix(iceURL, "stun:") &&
				!strings.HasPrefix(iceURL, "stuns:") &&
				!strings.HasPrefix(iceURL, "turn:") &&
				!strings.HasPrefix(iceURL, "turns:") {
				return config{}, fmt.Errorf("voice SFU: ICE server %d has an unsupported URL scheme", index)
			}
		}
	}

	if limit := strings.TrimSpace(getenv("CINNABAR_VOICE_MAX_PARTICIPANTS")); limit != "" {
		cfg.MaxParticipants, err = strconv.Atoi(limit)
		if err != nil || cfg.MaxParticipants < 1 || cfg.MaxParticipants > maxRoomLimit {
			return config{}, fmt.Errorf("voice SFU: participant limit must be between 1 and %d", maxRoomLimit)
		}
	}
	return cfg, nil
}

func decodePublicKey(encoded string) (ed25519.PublicKey, error) {
	decoders := []*base64.Encoding{
		base64.StdEncoding,
		base64.RawStdEncoding,
		base64.RawURLEncoding,
	}
	for _, decoder := range decoders {
		decoded, err := decoder.DecodeString(encoded)
		if err == nil && len(decoded) == ed25519.PublicKeySize {
			return ed25519.PublicKey(decoded), nil
		}
	}
	return nil, errors.New("expected a base64-encoded 32-byte Ed25519 public key")
}

func decodeControlToken(encoded string) ([]byte, error) {
	decoders := []*base64.Encoding{
		base64.StdEncoding,
		base64.RawStdEncoding,
		base64.RawURLEncoding,
	}
	for _, decoder := range decoders {
		decoded, err := decoder.DecodeString(encoded)
		if err == nil && len(decoded) == 32 {
			return decoded, nil
		}
	}
	return nil, errors.New("expected a base64-encoded 32-byte broker control token")
}

// sessionCloseHandler exposes one broker-only room teardown operation. The route is
// separately authenticated from WebRTC admission and never returns participant data.
func sessionCloseHandler(token []byte, closeSession func(roomID, sessionID string) int) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Cache-Control", "no-store")
		w.Header().Set("Pragma", "no-cache")
		provided := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
		if len(provided) != len(token) || subtle.ConstantTimeCompare([]byte(provided), token) != 1 {
			http.Error(w, "unauthorized", http.StatusUnauthorized)
			return
		}
		roomID := r.PathValue("roomID")
		sessionID := r.PathValue("sessionID")
		room, roomErr := uuid.Parse(roomID)
		session, sessionErr := uuid.Parse(sessionID)
		if roomErr != nil || sessionErr != nil || room.String() != roomID || session.String() != sessionID {
			http.Error(w, "invalid room or session identifier", http.StatusBadRequest)
			return
		}
		if r.ContentLength != 0 {
			http.Error(w, "request body must be empty", http.StatusBadRequest)
			return
		}
		_ = closeSession(roomID, sessionID)
		w.WriteHeader(http.StatusNoContent)
	})
}

func run(ctx context.Context, cfg config) error {
	sfu, err := voice.NewSFU(voice.SFUConfig{
		Admission: voice.VerifyConfig{
			Issuer: cfg.Issuer, Audience: cfg.Audience, PublicKey: cfg.PublicKey,
		},
		ICEServers: cfg.ICEServers,
		MaxParticipants: cfg.MaxParticipants,
	})
	if err != nil {
		return fmt.Errorf("voice SFU: initialize media router: %w", err)
	}
	defer sfu.Close()

	mux := http.NewServeMux()
	mux.Handle("/v1/voice", sfu)
	mux.Handle("POST /v1/admin/rooms/{roomID}/sessions/{sessionID}/close",
		sessionCloseHandler(cfg.ControlToken, sfu.CloseSession))
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		w.WriteHeader(http.StatusNoContent)
	})

	server := &http.Server{
		Addr:              cfg.ListenAddress,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
		IdleTimeout:       60 * time.Second,
		MaxHeaderBytes:    16 << 10,
		TLSConfig: &tls.Config{
			MinVersion: tls.VersionTLS12,
		},
	}
	serverDone := make(chan error, 1)
	go func() {
		serverDone <- server.ListenAndServeTLS(cfg.TLSCertFile, cfg.TLSKeyFile)
	}()

	slog.Info(
		"voice SFU listening",
		"address", cfg.ListenAddress,
		"issuer", cfg.Issuer,
		"audience", cfg.Audience,
		"max_room_participants", cfg.MaxParticipants,
		"ice_server_count", len(cfg.ICEServers),
	)

	select {
	case <-ctx.Done():
	case err := <-serverDone:
		if errors.Is(err, http.ErrServerClosed) {
			return nil
		}
		return fmt.Errorf("voice SFU: HTTPS listener failed: %w", err)
	}

	shutdownCtx, cancel := context.WithTimeout(context.Background(), shutdownTimeout)
	defer cancel()
	if err := server.Shutdown(shutdownCtx); err != nil {
		_ = server.Close()
		return fmt.Errorf("voice SFU: graceful shutdown failed: %w", err)
	}
	select {
	case err := <-serverDone:
		if !errors.Is(err, http.ErrServerClosed) && err != nil {
			return fmt.Errorf("voice SFU: HTTPS listener stopped unexpectedly: %w", err)
		}
	case <-shutdownCtx.Done():
		return fmt.Errorf("voice SFU: shutdown timed out: %w", shutdownCtx.Err())
	}
	return nil
}
