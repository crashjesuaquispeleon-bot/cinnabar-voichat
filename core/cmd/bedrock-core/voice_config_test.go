package main

import "testing"

func TestParseVoiceICEServers(t *testing.T) {
	tests := []struct {
		name    string
		raw     string
		wantErr bool
	}{
		{name: "missing", raw: "", wantErr: true},
		{name: "empty list", raw: "[]", wantErr: true},
		{name: "invalid JSON", raw: "{", wantErr: true},
		{name: "unsupported scheme", raw: `[{"urls":["https://stun.example.net"]}]`, wantErr: true},
		{name: "TURN without credentials", raw: `[{"urls":["turn:turn.example.net:3478"]}]`, wantErr: true},
		{name: "STUN", raw: `[{"urls":["stun:stun.example.net:3478"]}]`},
		{name: "TURN with credentials", raw: `[{"urls":["turns:turn.example.net:5349"],"username":"ephemeral-user","credential":"ephemeral-password"}]`},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			servers, err := parseVoiceICEServers(test.raw)
			if (err != nil) != test.wantErr {
				t.Fatalf("parseVoiceICEServers() error = %v, wantErr %v", err, test.wantErr)
			}
			if !test.wantErr && len(servers) != 1 {
				t.Fatalf("server count = %d, want 1", len(servers))
			}
		})
	}
}
