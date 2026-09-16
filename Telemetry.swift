// 匿名埋点与更新检查——两者都是跟同一个后端服务说话，所以放在一起。
//
// 从 AppMain.swift 拆出来，逻辑未改动。存储属性留在 AppMain.swift 的类体里。

import AppKit

extension AppDelegate {

    /// 匿名使用统计：**默认开启，菜单里可关**（2026-09-16 用户定的口径）。
    ///
    /// 只有"从没表态过"才按开启算：用户明确点过"关闭"的，`tb_tel_opt=false` 会一直留着，
    /// 不会被这次改默认值重新打开——改变默认值不该覆盖一个人已经做出的选择。
    ///
    /// 之前一版是"默认关闭 + 首次启动弹窗询问"：那对 opt-in 更友好，但用户要的是开箱即上报、
    /// 不打扰，所以回到 opt-out。代价写在 `about.html` 的隐私段落里（那里同时说明发什么、
    /// 不发什么、怎么关），免得用户看不到。
    func telEnabled() -> Bool {
        let d = UserDefaults.standard
        guard let v = d.object(forKey: "tb_tel_opt") as? Bool else { return true }   // 没表态过 → 开
        return v
    }
    func telURL() -> String {
        let custom = UserDefaults.standard.string(forKey: "tb_tel_url") ?? ""
        return custom.isEmpty ? Self.defaultTelURL : custom
    }
    func appVersion() -> String {
        Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "dev"
    }
    @objc func toggleTelemetry() {
        let d = UserDefaults.standard
        d.set(!telEnabled(), forKey: "tb_tel_opt")
        statusItem.menu = buildMenu()
        if telEnabled() { pingTelemetry() }
    }
    /// macOS 版本，例如 "15.3.1"（`operatingSystemVersion` 的三个整数）。
    var osVersionString: String {
        let v = ProcessInfo.processInfo.operatingSystemVersion
        return "\(v.majorVersion).\(v.minorVersion).\(v.patchVersion)"
    }

    /// Fire-and-forget ping, every 6 h: who (anonymous uid), which build, and the
    /// environment it runs in.
    ///
    /// 2026-09-16 扩过一次：加了 macOS 版本、芯片、语言/主题/数字显示模式、本次运行时长。
    /// 判断标准是"**是配置与环境，不是内容**"——没有任何 token 用量、文件名、项目名、
    /// 会话内容会离开这台机器；用量只有在你手动加入排行榜（另一个开关）时上传当日总量。
    /// 服务端会把这里每一个自由文本截断后再入库。
    func pingTelemetry() {
        guard telEnabled(), let url = URL(string: telURL() + "/api/ping") else { return }
        let body: [String: Any] = [
            "uid": lbUid(), "ver": appVersion(), "os": "macOS",
            "os_ver": osVersionString,
            "arch": {
                #if arch(arm64)
                return "arm64"
                #elseif arch(x86_64)
                return "x86_64"
                #else
                return "unknown"
                #endif
            }(),
            // 数字量级那套设置已经删了，所以不再上报 num_mode（服务端字段保留，
            // 老版本客户端仍会送，新版本就是空着）
            "lang": lang, "theme": themePref,
            "uptime_s": Int(Date().timeIntervalSince(Self.launchedAt)),
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: body) else { return }
        var req = URLRequest(url: url)
        req.httpMethod = "POST"
        req.httpBody = data
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.timeoutInterval = 5
        URLSession.shared.dataTask(with: req).resume()
    }

    @objc func checkUpdateManually() {
        checkUpdate { showHUDMenu in
            if showHUDMenu { self.refreshUpdateUI() }
        }
    }

    func checkUpdate(then: ((Bool) -> Void)? = nil) {
        guard let url = URL(string: telURL() + "/api/version?ver=\(appVersion())") else { return }
        URLSession.shared.dataTask(with: url) { [weak self] data, _, _ in
            guard let self, let data,
                  let d = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                  let latest = d["latest"] as? String else {
                DispatchQueue.main.async { then?(false) }
                return
            }
            let dl = d["url"] as? String ?? ""
            let file = d["file"] as? String ?? dl        // 老后端没有 file 字段时退回 url
            let sha = d["sha256"] as? String
            DispatchQueue.main.async {
                let has = latest.compare(self.appVersion(), options: .numeric) == .orderedDescending
                self.updateInfo = has ? (latest, dl, file) : nil
                self.updateSHA = has ? sha : nil
                self.refreshUpdateUI()
                then?(has)
            }
        }.resume()
    }
}
