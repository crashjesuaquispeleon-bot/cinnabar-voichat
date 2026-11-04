package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"strings"

	"github.com/pion/webrtc/v4"
)

// parseVoiceICEServers accepts only operator-provided ICE configuration.
// A missing or malformed value disables the voice bridge rather than silently
// creating a peer connection that cannot traverse NATs.
func parseVoiceICEServers(raw string) ([]webrtc.ICEServer, error) {
	raw = strings.TrimSpace(raw)
	if raw == "" {
		return nil, errors.New("CINNABAR_VOICE_ICE_SERVERS is required when voice WSS is enabled")
	}
	var servers []webrtc.ICEServer
	if err := json.Unmarshal([]byte(raw), &servers); err != nil {
		return nil, fmt.Errorf("invalid CINNABAR_VOICE_ICE_SERVERS JSON: %w", err)
	}
	if len(servers) == 0 {
		return nil, errors.New("CINNABAR_VOICE_ICE_SERVERS must contain at least one server")
	}
	for i, server := range servers {
		if len(server.URLs) == 0 {
			return nil, fmt.Errorf("ICE server %d has no URLs", i)
		}
		for _, rawURL := range server.URLs {
			u := strings.ToLower(strings.TrimSpace(rawURL))
			if !strings.HasPrefix(u, "stun:") && !strings.HasPrefix(u, "stuns:") &&
				!strings.HasPrefix(u, "turn:") && !strings.HasPrefix(u, "turns:") {
				return nil, fmt.Errorf("ICE server %d has an unsupported URL scheme", i)
			}
			if (strings.HasPrefix(u, "turn:") || strings.HasPrefix(u, "turns:")) &&
				(strings.TrimSpace(server.Username) == "" || strings.TrimSpace(server.Credential) == "") {
				return nil, fmt.Errorf("TURN server %d requires username and credential", i)
			}
		}
	}
	return servers, nil
}
