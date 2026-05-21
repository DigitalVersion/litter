import SwiftUI
import UIKit

/// Full-screen terminal. The Ghostty surface fills the entire body —
/// keystrokes go straight to the PTY via the hidden first-responder text
/// field, and the Esc/Ctrl/Tab/arrows row docks above the system keyboard
/// as an input accessory view (set up inside `GhosttyHostView`).
///
/// The header (backend chip + "Aa" appearance button) sits on top; we keep
/// the SSH trust banner as a transient overlay when needed.
struct TerminalScreen: View {
    let cwd: String?
    var preferredAlleycatNodeId: String? = nil

    @State private var controller = TerminalSessionController()
    @State private var backendOptions: [TerminalBackendOption] = []
    @State private var selectedBackendID: String?
    @State private var didStart = false
    @State private var terminalGridSize = TerminalGridSize(cols: 80, rows: 24)
    @State private var ghosttyRenderer = GhosttyTerminalRenderer()
    @State private var nativeRendererHasOutput = false
    @State private var showConfigSheet = false
    @AppStorage("litter.terminal.fontSize") private var storedFontSize: Double = 13.0
    @AppStorage("litter.terminal.themeId") private var storedThemeId: String = "litter-dark"
    @AppStorage("litter.terminal.cursorBlink") private var storedCursorBlink: Bool = true
    @Environment(\.scenePhase) private var scenePhase

    private let accent = Color(red: 0, green: 1, blue: 0.612)
    private let alleycatServerIdPrefix = "alleycat:"

    var body: some View {
        ZStack(alignment: .top) {
            terminalSurface
            backendBar
        }
        .background(Color.black.ignoresSafeArea())
        .ignoresSafeArea(.keyboard, edges: .bottom)
        .navigationTitle("Terminal")
        .navigationBarTitleDisplayMode(.inline)
        .toolbarColorScheme(.dark, for: .navigationBar)
        .toolbarBackground(Color.black, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .task {
            attachOutputSink()
            guard !didStart else { return }
            didStart = true
            let options = refreshBackendOptions()
            let initial = initialBackend(from: options, cwd: cwd)
            selectedBackendID = initial.id
            await controller.open(backend: initial.backend)
            applyConfigSettings()
        }
        .onReceive(NotificationCenter.default.publisher(for: .litterSavedServersDidChange)) { _ in
            reconcileBackendOptions()
        }
        .onChange(of: appSnapshotRevision) { _, _ in
            reconcileBackendOptions()
        }
        .onDisappear {
            // End any active first-responder hold so the keyboard tears
            // down and SwiftUI releases first responder. Without this the
            // keyboard can linger after navigating back, leaving the
            // parent screen unable to receive touches until the OS gives
            // up on the detached responder chain.
            UIApplication.shared.sendAction(
                #selector(UIResponder.resignFirstResponder),
                to: nil,
                from: nil,
                for: nil
            )
            controller.setOutputSink(nil)
            ghosttyRenderer.setFocused(false)
            ghosttyRenderer.setOccluded(true)
            controller.close()
        }
        .onChange(of: scenePhase) { _, newPhase in
            switch newPhase {
            case .active:
                ghosttyRenderer.setOccluded(false)
            case .inactive, .background:
                ghosttyRenderer.setOccluded(true)
            @unknown default:
                break
            }
        }
    }

    private var selectedBackend: TerminalBackendOption? {
        backendOptions.first { $0.id == selectedBackendID } ?? backendOptions.first
    }

    private var appSnapshotRevision: UInt64 {
        AppModel.shared.snapshotRevision
    }

    private func attachOutputSink() {
        let renderer = ghosttyRenderer
        controller.setOutputSink { data in
            Task { @MainActor in
                renderer.write(data)
            }
        }
    }

    private var backendBar: some View {
        HStack(spacing: 10) {
            Menu {
                ForEach(backendOptions) { option in
                    Button {
                        selectBackend(option)
                    } label: {
                        Label(option.title, systemImage: option.systemImage)
                    }
                }
                if backendOptions.count <= 1 {
                    Divider()
                    Button("No remote terminal servers") {}
                        .disabled(true)
                }
            } label: {
                HStack(spacing: 6) {
                    Image(systemName: selectedBackend?.systemImage ?? "terminal")
                    Text(selectedBackend?.title ?? "Local iSH")
                        .lineLimit(1)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 10, weight: .bold))
                }
                .font(.custom("SFMono-Regular", size: 12))
                .foregroundColor(accent)
                .padding(.horizontal, 10)
                .frame(height: 34)
                .background(Color.white.opacity(0.08))
                .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
            }

            Text(selectedBackend?.subtitle ?? "On device")
                .font(.custom("SFMono-Regular", size: 11))
                .foregroundColor(.white.opacity(0.48))
                .lineLimit(1)

            Spacer(minLength: 0)

            phaseChip

            Button {
                showConfigSheet = true
            } label: {
                Text("Aa")
                    .font(.custom("SFMono-Regular", size: 13))
                    .foregroundColor(accent)
                    .frame(width: 34, height: 30)
                    .background(Color.white.opacity(0.08))
                    .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
            }
            .accessibilityLabel("Theme and font")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
        .background(Color.black)
        .overlay(alignment: .bottom) {
            Rectangle()
                .fill(Color.white.opacity(0.08))
                .frame(height: 1)
        }
        .sheet(isPresented: $showConfigSheet) {
            TerminalConfigSheet(
                fontSize: $storedFontSize,
                themeId: $storedThemeId,
                cursorBlink: $storedCursorBlink,
                onApply: { applyConfigSettings() }
            )
        }
    }

    private var phaseChip: some View {
        HStack(spacing: 4) {
            Image(systemName: phaseIcon)
                .font(.system(size: 10, weight: .semibold))
            Text(phaseLabel)
                .font(.custom("SFMono-Regular", size: 11))
                .lineLimit(1)
        }
        .foregroundColor(phaseColor)
        .padding(.horizontal, 8)
        .frame(height: 22)
        .background(phaseColor.opacity(0.12))
        .clipShape(Capsule())
    }

    private var terminalSurface: some View {
        GeometryReader { geometry in
            ZStack(alignment: .topLeading) {
                GhosttyTerminalView(
                    renderer: ghosttyRenderer,
                    onNativeOutputVisibilityChanged: { visible in
                        nativeRendererHasOutput = visible
                    },
                    onInput: { data in
                        Task { await controller.send(data) }
                    },
                    onClearTapped: {
                        controller.clearOutput()
                        ghosttyRenderer.clearScreen()
                        nativeRendererHasOutput = false
                    },
                    onSendToAssistant: sendOutputToAssistant,
                    onFontSizePinched: { newSize in
                        storedFontSize = newSize
                        applyConfigSettings()
                    },
                    fontSize: storedFontSize
                )
                .frame(
                    width: geometry.size.width,
                    height: geometry.size.height,
                    alignment: .topLeading
                )
                .background(Color.black)

                if shouldShowStatusOverlay {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(displayText)
                            .font(.custom("SFMono-Regular", size: storedFontSize))
                            .foregroundColor(phaseColor)
                            .textSelection(.enabled)
                        if let challenge = controller.sshTrustChallenge {
                            Button {
                                Task { await controller.trustUnknownSshHostAndRetry() }
                            } label: {
                                Label("Trust \(challenge.fingerprint)", systemImage: "key.fill")
                                    .font(.custom("SFMono-Regular", size: 12))
                                    .foregroundColor(.black)
                                    .lineLimit(1)
                                    .truncationMode(.middle)
                                    .padding(.horizontal, 10)
                                    .frame(height: 32)
                                    .background(accent)
                                    .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
                            }
                            .buttonStyle(.plain)
                        }
                    }
                    .padding(.horizontal, 14)
                    .padding(.vertical, 12)
                    .padding(.top, TerminalChrome.backendBarHeight)
                }
            }
            .frame(
                width: geometry.size.width,
                height: geometry.size.height,
                alignment: .topLeading
            )
            .background(Color.black)
            .onAppear {
                applyConfigSettings()
                resizeTerminal(for: geometry.size)
            }
            .onChange(of: geometry.size) { _, size in
                resizeTerminal(for: size)
            }
            .onChange(of: storedFontSize) { _, _ in
                applyConfigSettings()
                // Font size change re-grids the PTY but the SwiftUI size
                // didn't move — re-evaluate against the same geometry on
                // the next frame so the new cell metrics propagate.
                DispatchQueue.main.async {
                    resizeTerminal(for: geometry.size)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private var displayText: String {
        if !controller.output.isEmpty {
            return controller.output
        }
        switch controller.phase {
        case .idle, .connecting:
            return "Connecting...\n"
        case .running:
            return ""
        case .exited(let code):
            return "\n[process exited \(code)]\n"
        case .failed(let message):
            return "\n[terminal failed: \(message)]\n"
        }
    }

    private var shouldShowStatusOverlay: Bool {
        if !controller.output.isEmpty {
            return !nativeRendererHasOutput
        }
        switch controller.phase {
        case .idle, .connecting, .failed, .exited:
            return true
        case .running:
            return false
        }
    }

    private var phaseIcon: String {
        switch controller.phase {
        case .idle, .connecting: return "circle.dotted"
        case .running: return "terminal"
        case .exited: return "checkmark.circle"
        case .failed: return "exclamationmark.triangle"
        }
    }

    private var phaseColor: Color {
        switch controller.phase {
        case .idle, .connecting: return .white.opacity(0.45)
        case .running: return accent
        case .exited: return .white.opacity(0.5)
        case .failed: return .red
        }
    }

    private var phaseLabel: String {
        switch controller.phase {
        case .idle: return "idle"
        case .connecting: return "connecting"
        case .running: return selectedBackend?.runningLabel ?? "running"
        case .exited(let code): return "exited \(code)"
        case .failed: return "failed"
        }
    }

    /// Forward terminal text to the assistant on the current active
    /// thread. If a painted selection is active, prefer that text; else
    /// send the visible viewport.
    private func sendOutputToAssistant() {
        guard let threadKey = AppModel.shared.snapshot?.activeThread else { return }
        let selection = ghosttyRenderer.readSelection()?.trimmingCharacters(in: .whitespacesAndNewlines)
        let fallback = controller.output.trimmingCharacters(in: .whitespacesAndNewlines)
        let payload = (selection?.isEmpty == false ? selection : nil) ?? fallback
        guard !payload.isEmpty else { return }
        Task {
            try? await ghosttyRenderer.sendTextToAssistant(
                store: AppModel.shared.store,
                threadKey: threadKey,
                selection: payload
            )
        }
    }

    private func initialBackend(
        from options: [TerminalBackendOption],
        cwd: String?
    ) -> TerminalBackendOption {
        if let preferredNodeId = normalized(preferredAlleycatNodeId),
           let match = options.first(where: { $0.alleycatNodeId == preferredNodeId }) {
            return match
        }
        return options.first ?? TerminalBackendOption.localIsh(cwd: cwd)
    }

    private func selectBackend(_ option: TerminalBackendOption) {
        guard selectedBackendID != option.id else { return }
        selectedBackendID = option.id
        attachOutputSink()
        ghosttyRenderer.clearScreen()
        nativeRendererHasOutput = false
        Task {
            await controller.switchBackend(option.backend)
        }
    }

    /// Recompute PTY cols/rows. Prefer the renderer's live cell metrics —
    /// they're driven by Ghostty's actual font measurement so font-size
    /// changes, rotation, and keyboard show/hide all yield correct grids.
    /// Falls back to a font-size-aware estimate only when the renderer
    /// hasn't yet reported metrics (first frame of attach).
    private func resizeTerminal(for size: CGSize) {
        let scale = UIScreen.main.scale
        let metrics = ghosttyRenderer.cellMetrics()
        let grid: TerminalGridSize
        if let metrics, metrics.cellWidthPx > 0, metrics.cellHeightPx > 0 {
            grid = TerminalGridSize(
                size: size,
                contentScale: scale,
                cellWidthPx: CGFloat(metrics.cellWidthPx),
                cellHeightPx: CGFloat(metrics.cellHeightPx)
            )
        } else {
            grid = TerminalGridSize(estimatedFor: size, fontSize: storedFontSize)
        }
        ghosttyRenderer.setGridSize(cols: grid.cols, rows: grid.rows)
        guard grid != terminalGridSize else { return }
        terminalGridSize = grid
        let notifyBackend = selectedBackend?.supportsResize == true
        Task {
            await controller.resize(cols: grid.cols, rows: grid.rows, notifyBackend: notifyBackend)
        }
    }

    private func loadBackendOptions(cwd: String?) -> [TerminalBackendOption] {
        var options = [TerminalBackendOption.localIsh(cwd: cwd)]
        var seenNodeIds = Set<String>()
        var seenSshKeys = Set<String>()
        let savedServers = SavedServerStore.load()
        let savedByNodeId = savedServers.reduce(into: [String: SavedServer]()) { result, saved in
            guard let nodeId = normalized(saved.alleycatNodeId),
                  result[nodeId] == nil else {
                return
            }
            result[nodeId] = saved
        }
        for server in AppModel.shared.snapshot?.servers ?? [] {
            guard let nodeId = alleycatNodeId(fromServerId: server.serverId),
                  seenNodeIds.insert(nodeId).inserted,
                  let token = try? AlleycatCredentialStore.shared.loadToken(nodeId: nodeId),
                  !token.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else {
                continue
            }
            let saved = savedByNodeId[nodeId]
            options.append(
                TerminalBackendOption.remoteAlleycat(
                    name: normalized(saved?.name) ?? server.displayName,
                    nodeId: nodeId,
                    token: token,
                    relay: normalized(saved?.alleycatRelay)
                )
            )
        }
        // Include non-remembered discovered records too: if they have a
        // terminal-capable credential, the chooser should be able to switch
        // to them while the app still knows about the connection.
        for saved in savedServers {
            if let nodeId = normalized(saved.alleycatNodeId),
               seenNodeIds.insert(nodeId).inserted,
               let token = try? AlleycatCredentialStore.shared.loadToken(nodeId: nodeId),
               !token.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                options.append(
                    TerminalBackendOption.remoteAlleycat(
                        name: saved.name,
                        nodeId: nodeId,
                        token: token,
                        relay: normalized(saved.alleycatRelay)
                    )
                )
                continue
            }

            let host = saved.hostname
            let sshPort = saved.sshPort ?? 22
            let sshKey = "\(host.lowercased()):\(sshPort)"
            guard !host.isEmpty, seenSshKeys.insert(sshKey).inserted else { continue }
            guard let credential = (try? SSHCredentialStore.shared.load(host: host, port: Int(sshPort))) ?? nil,
                  let sshAuth = Self.terminalSshAuth(from: credential) else {
                continue
            }
            options.append(
                TerminalBackendOption.remoteSsh(
                    name: saved.name,
                    host: host,
                    port: sshPort,
                    username: credential.username,
                    auth: sshAuth
                )
            )
        }
        return options
    }

    @discardableResult
    private func refreshBackendOptions() -> [TerminalBackendOption] {
        let options = loadBackendOptions(cwd: cwd)
        backendOptions = options
        return options
    }

    private func reconcileBackendOptions() {
        let options = refreshBackendOptions()
        guard let selectedBackendID,
              options.contains(where: { $0.id == selectedBackendID }) else {
            self.selectedBackendID = initialBackend(from: options, cwd: cwd).id
            return
        }
    }

    private static func terminalSshAuth(from credential: SavedSSHCredential) -> TerminalSshAuth? {
        switch credential.method {
        case .password:
            guard let password = credential.password, !password.isEmpty else { return nil }
            return .password(password: password)
        case .key:
            guard let key = credential.privateKey, !key.isEmpty else { return nil }
            return .privateKey(keyPem: key, passphrase: credential.passphrase)
        }
    }

    private func normalized(_ value: String?) -> String? {
        let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return trimmed.isEmpty ? nil : trimmed
    }

    private func alleycatNodeId(fromServerId serverId: String) -> String? {
        let trimmed = serverId.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix(alleycatServerIdPrefix) else { return nil }
        return normalized(String(trimmed.dropFirst(alleycatServerIdPrefix.count)))
    }

    private func applyConfigSettings() {
        let config = TerminalConfig(
            theme: TerminalThemeChoice.preset(forId: storedThemeId),
            fontFamily: "SFMono-Regular",
            fontSizePt: Float(storedFontSize),
            cursorStyle: .bar,
            cursorBlink: storedCursorBlink,
            scrollbackLines: 10_000
        )
        ghosttyRenderer.applyConfig(config)
    }
}

private enum TerminalChrome {
    static let backendBarHeight: CGFloat = 51
}

private enum TerminalThemeChoice: String, CaseIterable, Identifiable {
    case litterDark = "litter-dark"
    case catppuccinFrappe = "catppuccin-frappe"
    case catppuccinFrappeLight = "catppuccin-frappe-light"
    case solarizedDark = "solarized-dark"
    case solarizedLight = "solarized-light"

    var id: String { rawValue }

    var title: String {
        switch self {
        case .litterDark: return "Litter Dark"
        case .catppuccinFrappe: return "Catppuccin Frappé"
        case .catppuccinFrappeLight: return "Catppuccin Frappé Light"
        case .solarizedDark: return "Solarized Dark"
        case .solarizedLight: return "Solarized Light"
        }
    }

    var preset: TerminalThemePreset {
        switch self {
        case .litterDark: return .litterDark
        case .catppuccinFrappe: return .catppuccinFrappe
        case .catppuccinFrappeLight: return .catppuccinFrappeLight
        case .solarizedDark: return .solarized(dark: true)
        case .solarizedLight: return .solarized(dark: false)
        }
    }

    static func preset(forId id: String) -> TerminalThemePreset {
        (TerminalThemeChoice(rawValue: id) ?? .litterDark).preset
    }
}

private struct TerminalConfigSheet: View {
    @Binding var fontSize: Double
    @Binding var themeId: String
    @Binding var cursorBlink: Bool
    let onApply: () -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                Section("Font") {
                    HStack {
                        Text("Size")
                            .font(.custom("SFMono-Regular", size: 13))
                        Spacer()
                        Text("\(Int(fontSize)) pt")
                            .font(.custom("SFMono-Regular", size: 13))
                            .foregroundColor(.secondary)
                    }
                    Slider(value: $fontSize, in: 10...24, step: 1) {
                        Text("Font size")
                    }
                    .onChange(of: fontSize) { _, _ in onApply() }
                }
                Section("Theme") {
                    Picker("Theme", selection: $themeId) {
                        ForEach(TerminalThemeChoice.allCases) { choice in
                            Text(choice.title).tag(choice.id)
                        }
                    }
                    .pickerStyle(.inline)
                    .onChange(of: themeId) { _, _ in onApply() }
                }
                Section("Cursor") {
                    Toggle("Blink", isOn: $cursorBlink)
                        .onChange(of: cursorBlink) { _, _ in onApply() }
                }
            }
            .navigationTitle("Terminal")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }
}

private struct TerminalBackendOption: Identifiable, Hashable {
    let id: String
    let title: String
    let subtitle: String
    let systemImage: String
    let alleycatNodeId: String?
    let supportsResize: Bool
    let runningLabel: String
    let backend: TerminalBackendKind

    static func localIsh(cwd: String?) -> TerminalBackendOption {
        TerminalBackendOption(
            id: "local-ish",
            title: "Local iSH",
            subtitle: cwd?.isEmpty == false ? cwd! : "/root",
            systemImage: "iphone",
            alleycatNodeId: nil,
            supportsResize: true,
            runningLabel: "running",
            backend: .localIsh(cwd: normalized(cwd))
        )
    }

    static func remoteAlleycat(
        name: String,
        nodeId: String,
        token: String,
        relay: String?
    ) -> TerminalBackendOption {
        TerminalBackendOption(
            id: "alleycat-\(nodeId)",
            title: name.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "Remote shell" : name,
            subtitle: shortNodeId(nodeId),
            systemImage: "server.rack",
            alleycatNodeId: nodeId,
            supportsResize: true,
            runningLabel: "remote",
            backend: .remoteAlleycat(
                nodeId: nodeId,
                token: token,
                relay: relay,
                shell: nil
            )
        )
    }

    static func remoteSsh(
        name: String,
        host: String,
        port: UInt16,
        username: String,
        auth: TerminalSshAuth
    ) -> TerminalBackendOption {
        let trimmedName = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let title = trimmedName.isEmpty ? "\(username)@\(host)" : trimmedName
        return TerminalBackendOption(
            id: "ssh-\(host.lowercased()):\(port)",
            title: title,
            subtitle: "ssh \(username)@\(host):\(port)",
            systemImage: "terminal.fill",
            alleycatNodeId: nil,
            supportsResize: true,
            runningLabel: "ssh",
            backend: .remoteSsh(
                host: host,
                port: port,
                username: username,
                auth: auth,
                shell: nil,
                acceptUnknownHost: false,
                cwd: nil
            )
        )
    }

    private static func normalized(_ value: String?) -> String? {
        let trimmed = value?.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        return trimmed.isEmpty ? nil : trimmed
    }

    private static func shortNodeId(_ raw: String) -> String {
        raw.count <= 16 ? raw : "\(raw.prefix(8))...\(raw.suffix(8))"
    }
}

private struct TerminalGridSize: Equatable {
    let cols: UInt16
    let rows: UInt16

    init(cols: UInt16, rows: UInt16) {
        self.cols = cols
        self.rows = rows
    }

    /// Derive cols/rows from the live cell metrics Ghostty reports. Pixel
    /// values come from `ghostty_surface_size`, view bounds come from
    /// SwiftUI; divide the latter (in pixels) by the former to get a
    /// grid that lines up exactly with what Ghostty paints.
    init(size: CGSize, contentScale: CGFloat, cellWidthPx: CGFloat, cellHeightPx: CGFloat) {
        let widthPx = max(0, size.width * contentScale)
        let heightPx = max(0, size.height * contentScale)
        let computedCols = Int(floor(widthPx / max(cellWidthPx, 1)))
        let computedRows = Int(floor(heightPx / max(cellHeightPx, 1)))
        cols = UInt16(max(20, min(240, computedCols)))
        rows = UInt16(max(4, min(120, computedRows)))
    }

    /// First-frame fallback used before the renderer has produced metrics.
    /// Estimate cell dimensions from the chosen font size so the initial
    /// PTY grid is in the right ballpark across the 10–24 pt range.
    init(estimatedFor size: CGSize, fontSize: Double) {
        let cellWidth = max(6.0, fontSize * 0.6)
        let cellHeight = max(12.0, fontSize * 1.31)
        let contentWidth = max(0, size.width)
        let contentHeight = max(0, size.height)
        let computedCols = Int(contentWidth / cellWidth)
        let computedRows = Int(contentHeight / cellHeight)
        cols = UInt16(max(20, min(240, computedCols)))
        rows = UInt16(max(4, min(120, computedRows)))
    }
}

#if DEBUG
#Preview("Terminal") {
    NavigationStack {
        TerminalScreen(cwd: "/root")
    }
}
#endif
