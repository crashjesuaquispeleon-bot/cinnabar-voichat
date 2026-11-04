package proxy

import (
	"bytes"
	"encoding/json"
	"errors"
	"log/slog"
	"strings"
	"testing"

	"github.com/sandertv/gophertunnel/minecraft/p2p"
	"github.com/sandertv/gophertunnel/minecraft/room"
)

func TestReconcileNoncesIssuesOnePerMemberAndForgetsLeavers(t *testing.T) {
	nonces := map[string]string{}
	if !reconcileNonces(nonces, []string{"host", "a", "b", ""}, "host") {
		t.Fatal("new members did not change nonces")
	}
	if _, ok := nonces["host"]; ok || len(nonces) != 2 || nonces["a"] == "" || nonces["a"] == nonces["b"] {
		t.Fatalf("nonces = %v", nonces)
	}
	first := nonces["a"]
	if reconcileNonces(nonces, []string{"host", "a", "b"}, "host") {
		t.Fatal("unchanged membership rewrote nonces")
	}
	if nonces["a"] != first {
		t.Fatal("a staying member's nonce changed")
	}
	if !reconcileNonces(nonces, []string{"host", "b"}, "host") {
		t.Fatal("a leaver did not change nonces")
	}
	if _, ok := nonces["a"]; ok {
		t.Fatal("a leaver's nonce stayed replayable")
	}
	reconcileNonces(nonces, []string{"host", "a", "b"}, "host")
	if nonces["a"] == first {
		t.Fatal("a returning member reused its old nonce")
	}
}

func TestAdmitFriendNeedsTheIssuedNonceAndRefusesTheHost(t *testing.T) {
	for _, test := range []struct {
		name            string
		xuid, presented string
		issued          string
		ok, want        bool
	}{
		{"issued nonce", "2", "n", "n", true, true},
		{"wrong nonce", "2", "x", "n", true, false},
		{"no nonce issued", "2", "n", "", false, false},
		{"empty presented", "2", "", "", true, false},
		{"host account", "1", "n", "n", true, false},
		{"no xuid", "", "n", "n", true, false},
	} {
		if _, got := admitFriend("1", test.xuid, test.presented, test.issued, test.ok); got != test.want {
			t.Errorf("%s: admitted = %v, want %v", test.name, got, test.want)
		}
	}
}

// Vanilla clients decode the world card and wait for their nonce under "nonces".
func TestFriendSessionPropertiesDecodeAsAVanillaWorld(t *testing.T) {
	properties := friendSessionProperties{
		Status: room.Status{
			HostName: "Host", OwnerID: "1", WorldName: "My World", MemberCount: 1,
			BroadcastSetting: p2p.BroadcastSettingFriendsOfFriends, TransportLayer: p2p.TransportLayerNetherNet,
			SupportedConnections: []p2p.Connection{{Type: p2p.ConnectionTypeSignalingOverJSONRPC, NetherNetID: "42"}},
		},
		Nonces: map[string]string{"2": "n"},
	}
	encoded, err := json.Marshal(properties)
	if err != nil {
		t.Fatal(err)
	}
	var world p2p.World
	if err := json.Unmarshal(encoded, &world); err != nil {
		t.Fatal(err)
	}
	if world.WorldName != "My World" || world.OwnerID != "1" || world.Nonces["2"] != "n" {
		t.Fatalf("decoded world = %+v", world)
	}
	if !world.Listed("2", false, func(string) bool { return false }) {
		t.Fatal("friends of friends cannot see the world")
	}
}

// A friend's ended session is logged without the friend's XUID or the addresses in its error.
func TestFriendHostVoiceRosterReferenceCountsDuplicateConnections(t *testing.T) {
	host := &FriendHost{voiceMembers: make(map[string]int)}
	first := "2535400000000002"
	second := "2535400000000003"

	host.addPlayer(first, 1)
	host.addPlayer(first, 1)
	host.addPlayer(second, 1)
	host.addPlayer("not-an-xuid", 1)

	want := first + "," + second
	if got := strings.Join(host.voiceRoster(), ","); got != want {
		t.Fatalf("voice roster = %q, want %q", got, want)
	}

	// One of two live connections for the same authenticated account ends.
	host.addPlayer(first, -1)
	if got := strings.Join(host.voiceRoster(), ","); got != want {
		t.Fatalf("roster after one duplicate disconnect = %q, want %q", got, want)
	}

	// The final connection ending removes that account, but not the other guest.
	host.addPlayer(first, -1)
	if got := strings.Join(host.voiceRoster(), ","); got != second {
		t.Fatalf("roster after final duplicate disconnect = %q, want %q", got, second)
	}
	host.addPlayer(second, -1)
	if got := host.voiceRoster(); len(got) != 0 {
		t.Fatalf("roster after all guests disconnect = %v, want empty", got)
	}
}

func TestFriendSessionEndNamesNoFriend(t *testing.T) {
	var output bytes.Buffer
	log := slog.New(slog.NewTextHandler(&output, &slog.HandlerOptions{Level: slog.LevelDebug}))
	logFriendSessionEnded(log, "2535400000000000", errors.New("read udp 198.51.100.4:7551: connection reset"))
	line := output.String()
	for _, private := range []string{"2535400000000000", "198.51.100"} {
		if strings.Contains(line, private) {
			t.Fatalf("log line %q holds %q", line, private)
		}
	}
	if !strings.Contains(line, "xuid=[redacted:") || !strings.Contains(line, "connection reset") {
		t.Fatalf("log line %q lost its hashed XUID or error", line)
	}
}
