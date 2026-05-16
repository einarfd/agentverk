import Testing
@testable import AvfRunnerCore

@Suite("LeaseLookup")
struct LeaseLookupTests {
    /// Single matching lease — trivial happy path.
    @Test
    func hostnameLookupSingleMatch() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.5
                hw_address=1,52:55:55:54:f4:e6
                identifier=1,52:55:55:54:f4:e6
                lease=0x6a00d433
            }
            """
        #expect(LeaseLookup.parse(contents, byHostname: "foobar") == "192.168.205.5")
    }

    /// Regression: bootpd writes lease entries sorted by IP, NOT by recency.
    /// When a hostname has been used across multiple VM incarnations,
    /// several blocks share `name=<host>` and the freshest one is the one
    /// with the highest `lease=` timestamp — not the last in file order.
    ///
    /// This fixture replicates the real failure we hit during the AVF
    /// backend work: file lists 192.168.205.38 (newest), .37, .36, .32
    /// in descending-IP order. The pre-fix parser returned .32 (last
    /// match seen) and SSH connected to a stale IP no host was at; the
    /// fixed parser must return .38.
    @Test
    func hostnameLookupReturnsFreshestNotLastInFileOrder() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.38
                hw_address=ff,f1:f5:dd:7f:0:2:0:0:ab:11:f4:da:bb:51:6d:75:1e:9a
                identifier=ff,f1:f5:dd:7f:0:2:0:0:ab:11:f4:da:bb:51:6d:75:1e:9a
                lease=0x6a05ad86
            }
            {
                name=foobar
                ip_address=192.168.205.37
                hw_address=ff,f1:f5:dd:7f:0:2:0:0:ab:11:9d:3c:85:91:8:a3:de:e7
                identifier=ff,f1:f5:dd:7f:0:2:0:0:ab:11:9d:3c:85:91:8:a3:de:e7
                lease=0x6a059fdd
            }
            {
                name=foobar
                ip_address=192.168.205.36
                hw_address=ff,f1:f5:dd:7f:0:2:0:0:ab:11:f3:16:54:50:9:b2:4e:67
                identifier=ff,f1:f5:dd:7f:0:2:0:0:ab:11:f3:16:54:50:9:b2:4e:67
                lease=0x6a04fa80
            }
            {
                name=foobar
                ip_address=192.168.205.32
                hw_address=ff,f1:f5:dd:7f:0:2:0:0:ab:11:b0:83:19:e4:79:95:a8:f1
                identifier=ff,f1:f5:dd:7f:0:2:0:0:ab:11:b0:83:19:e4:79:95:a8:f1
                lease=0x6a05abf6
            }
            """
        #expect(LeaseLookup.parse(contents, byHostname: "foobar") == "192.168.205.38")
    }

    /// Other hostnames are interleaved between foobar entries — the
    /// parser must still pick the freshest foobar lease, not whatever's
    /// adjacent in the file.
    @Test
    func hostnameLookupIgnoresInterleavedOtherHostnames() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.10
                lease=0x6a000010
            }
            {
                name=other
                ip_address=192.168.205.9
                lease=0xffffffff
            }
            {
                name=foobar
                ip_address=192.168.205.8
                lease=0x6a000020
            }
            """
        #expect(LeaseLookup.parse(contents, byHostname: "foobar") == "192.168.205.8")
    }

    @Test
    func hostnameLookupReturnsNilWhenNoMatch() {
        let contents = """
            {
                name=other
                ip_address=192.168.205.5
                lease=0x6a00d433
            }
            """
        #expect(LeaseLookup.parse(contents, byHostname: "foobar") == nil)
    }

    /// MAC lookup strips bootpd's `1,` ARP-type prefix before comparison.
    @Test
    func macLookupStripsArpPrefix() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.5
                hw_address=1,52:55:55:54:f4:e6
                lease=0x6a00d433
            }
            """
        #expect(
            LeaseLookup.parse(contents, byMac: "52:55:55:54:f4:e6") == "192.168.205.5"
        )
    }

    /// MAC comparison is case-insensitive.
    @Test
    func macLookupCaseInsensitive() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.5
                hw_address=1,52:55:55:54:F4:E6
                lease=0x6a00d433
            }
            """
        #expect(
            LeaseLookup.parse(contents, byMac: "52:55:55:54:f4:e6") == "192.168.205.5"
        )
    }

    /// Same recency rule applies to MAC matches.
    @Test
    func macLookupReturnsFreshestNotLastInFileOrder() {
        let contents = """
            {
                name=earlier
                ip_address=192.168.205.10
                hw_address=1,52:55:55:54:f4:e6
                lease=0x6a000050
            }
            {
                name=later
                ip_address=192.168.205.8
                hw_address=1,52:55:55:54:f4:e6
                lease=0x6a000010
            }
            """
        #expect(
            LeaseLookup.parse(contents, byMac: "52:55:55:54:f4:e6") == "192.168.205.10"
        )
    }

    /// `findGuestIp` returns nil when the lease file doesn't exist —
    /// callers treat nil as "guest IP not yet known."
    /// (We can't easily exercise the full file-read happy path here
    /// without `import Foundation`, which fails to build on Command
    /// Line Tools-only setups due to a missing `_Testing_Foundation`
    /// cross-import module. The parser is exercised exhaustively via
    /// `parse(_:byHostname:)` and `parse(_:byMac:)` above, which is
    /// where the real logic lives — `findGuestIp` itself is a thin
    /// `String(contentsOfFile:)` wrapper.)
    @Test
    func findGuestIpReturnsNilWhenFileMissing() {
        let ip = LeaseLookup.findGuestIp(
            hostname: "foobar",
            mac: nil,
            leaseFilePath: "/nonexistent/path/that/should/not/exist.leases"
        )
        #expect(ip == nil)
    }

    /// Block with no `lease=` field shouldn't poison the freshest-wins
    /// comparison: leaseTimestamp returns 0 for missing/unparseable
    /// values, so a real lease always beats it. But if it's the ONLY
    /// match, we still return it — a stale-but-existing record is more
    /// useful than nil.
    @Test
    func leaseWithMissingTimestampStillReturnedIfOnlyMatch() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.5
            }
            """
        #expect(LeaseLookup.parse(contents, byHostname: "foobar") == "192.168.205.5")
    }

    @Test
    func leaseWithMissingTimestampLosesToOneWithTimestamp() {
        let contents = """
            {
                name=foobar
                ip_address=192.168.205.5
            }
            {
                name=foobar
                ip_address=192.168.205.6
                lease=0x6a00d433
            }
            """
        #expect(LeaseLookup.parse(contents, byHostname: "foobar") == "192.168.205.6")
    }
}
