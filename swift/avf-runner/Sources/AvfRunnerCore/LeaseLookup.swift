// Look up an AVF guest's IP address from the host DHCP lease file.
//
// Apple Virtualization NAT mode runs `bootpd` on the bridge interface
// it provisions for guests; bootpd writes leases to a plain-text file
// at /var/db/dhcpd_leases.
//
// Lease file format (one entry per VM):
//
//   {
//       name=avf-test
//       ip_address=192.168.205.4
//       hw_address=1,52:55:55:54:f4:e6
//       identifier=1,52:55:55:54:f4:e6
//       lease=0x6a00d433
//   }
//
// We key on `name` (which is the guest's DHCP-advertised hostname,
// set by cloud-init's `local-hostname` to the VM name) rather than
// `hw_address`. Reason: many modern Linux DHCP clients send an RFC
// 4361 client identifier (17-byte DUID-based blob) instead of the raw
// MAC, and bootpd writes that in `hw_address` — making MAC-based
// lookup fail for cloud images using systemd-networkd. Hostname is
// always populated from cloud-init and matches the agv VM name 1:1.
//
// MAC-based lookup remains as a fallback for guests that don't send
// a hostname for some reason.

import Foundation

public enum LeaseLookup {
    /// Path bootpd writes to. Constant on macOS.
    public static let defaultPath = "/var/db/dhcpd_leases"

    /// Find the most recent IP leased to a guest with the given DHCP
    /// hostname. Optional `mac` is used as a secondary match when the
    /// hostname doesn't appear in the leases file. Returns nil if
    /// nothing matches yet — callers should treat nil as "not yet
    /// known" and poll briefly after VM start.
    public static func findGuestIp(
        hostname: String,
        mac: String? = nil,
        leaseFilePath: String = defaultPath
    ) -> String? {
        guard let contents = try? String(contentsOfFile: leaseFilePath, encoding: .utf8) else {
            return nil
        }
        if let ip = parse(contents, byHostname: hostname) {
            return ip
        }
        if let mac, let ip = parse(contents, byMac: mac) {
            return ip
        }
        return nil
    }

    /// Pure parser — match the freshest lease (highest `lease=`
    /// timestamp) whose `name` field equals `hostname` (case-sensitive;
    /// bootpd preserves case).
    public static func parse(_ contents: String, byHostname hostname: String) -> String? {
        return iterateLeases(contents) { fields in
            fields["name"] == hostname ? fields["ip_address"] : nil
        }
    }

    /// Pure parser — match the freshest lease (highest `lease=`
    /// timestamp) whose `hw_address` field, with the ARP-type prefix
    /// stripped, equals `mac` case-insensitively.
    public static func parse(_ contents: String, byMac mac: String) -> String? {
        let target = mac.lowercased()
        return iterateLeases(contents) { fields in
            guard let raw = fields["hw_address"] else { return nil }
            let stripped: String
            if let comma = raw.firstIndex(of: ",") {
                stripped = String(raw[raw.index(after: comma)...])
            } else {
                stripped = raw
            }
            return stripped.lowercased() == target ? fields["ip_address"] : nil
        }
    }

    /// Parse the `lease=` field as a hex Unix timestamp. bootpd writes
    /// it as `0x6a05ad86` (no decimal form observed). Returns 0 for
    /// missing/unparseable values so they sort below any real lease —
    /// a real lease is always better than no timestamp at all.
    private static func leaseTimestamp(_ fields: [String: String]) -> UInt64 {
        guard let raw = fields["lease"] else { return 0 }
        let trimmed = raw.hasPrefix("0x") ? String(raw.dropFirst(2)) : raw
        return UInt64(trimmed, radix: 16) ?? 0
    }

    /// Walk every `{ ... }` block in the leases file, yielding each
    /// block's parsed key→value map to `extract`. Returns the IP from
    /// the block with the highest `lease=` timestamp where `extract`
    /// returned non-nil.
    ///
    /// bootpd does NOT append fresh leases after stale ones — entries
    /// are written sorted by IP (descending in practice), so simply
    /// taking the last hit in file order returns whichever match
    /// happens to have the lowest IP, not the freshest. Compare
    /// `lease=` timestamps to pick the right one when a hostname has
    /// been used across multiple VM incarnations.
    private static func iterateLeases(
        _ contents: String,
        extract: ([String: String]) -> String?
    ) -> String? {
        var best: String? = nil
        var bestTs: UInt64 = 0
        var current: [String: String] = [:]
        var inBlock = false
        for rawLine in contents.components(separatedBy: .newlines) {
            let line = rawLine.trimmingCharacters(in: .whitespaces)
            if line == "{" {
                inBlock = true
                current = [:]
                continue
            }
            if line == "}" {
                if inBlock, let hit = extract(current) {
                    let ts = leaseTimestamp(current)
                    if best == nil || ts > bestTs {
                        best = hit
                        bestTs = ts
                    }
                }
                inBlock = false
                continue
            }
            guard inBlock else { continue }
            guard let eq = line.firstIndex(of: "=") else { continue }
            current[String(line[..<eq])] = String(line[line.index(after: eq)...])
        }
        return best
    }
}
