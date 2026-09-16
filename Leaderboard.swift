// 全球排行榜：opt-in、只上传当日总量、只显示自己的名次。
//
// 从 AppMain.swift 拆出来，逻辑未改动。存储属性留在 AppMain.swift 的类体里。

import AppKit

extension AppDelegate {
    // MARK: - Global leaderboard (opt-in, aggregated, anonymous)

    func lbEnabled() -> Bool { UserDefaults.standard.bool(forKey: "tb_opt_in") }
    /// 排行榜现在就在埋点那个服务里（同一个 Rust 容器），不再有第二个进程；
    /// `tb_lb_url` 只在开发时用来指向本地容器，生产留空即用 `telURL()`。
    func lbURL() -> String {
        let custom = UserDefaults.standard.string(forKey: "tb_lb_url") ?? ""
        return custom.isEmpty ? telURL() : custom
    }

    /// 排行榜的时间范围：1 / 7 / 30 天，0 = 全部。与网页顶部那排预设同一套口径。
    func lbDays() -> Int {
        let d = UserDefaults.standard.object(forKey: "tb_lb_days") as? Int ?? 7
        return [1, 7, 30, 0].contains(d) ? d : 7
    }
    func lbRangeLabel() -> String {
        switch lbDays() {
        case 1:  return L("今天", "Today")
        case 7:  return L("近 7 天", "Last 7d")
        case 30: return L("近 30 天", "Last 30d")
        default: return L("全部", "All time")
        }
    }
    @objc func setLbDays(_ sender: NSMenuItem) {
        UserDefaults.standard.set(sender.tag, forKey: "tb_lb_days")
        statusItem.menu = buildMenu()
        lbTick()
    }
    func lbUid() -> String {
        if let u = UserDefaults.standard.string(forKey: "tb_uid") { return u }
        let u = "u-" + UUID().uuidString.prefix(8).lowercased()
        UserDefaults.standard.set(u, forKey: "tb_uid")
        return u
    }
    func lbName() -> String {
        let n = UserDefaults.standard.string(forKey: "tb_name") ?? ""
        return n.isEmpty ? L("燃烧者-", "burner-") + String(lbUid().suffix(4)) : n
    }

    @objc func toggleLB() {
        let d = UserDefaults.standard
        d.set(!d.bool(forKey: "tb_opt_in"), forKey: "tb_opt_in")
        statusItem.menu = buildMenu()
        lbTick()
    }

    func lbTick() {
        guard lbEnabled() else {
            hudRank?.stringValue = ""
            hudRank?.isHidden = true
            return
        }
        let agg = aggregate()
        let t = agg.todayTotal
        // saturating throughout: `aggregate()` can now legitimately hit the
        // ceiling, and `Int` arithmetic traps rather than wrapping
        // 口径与挂件、网页一致：输入 + 输出（早先是"输出 + 非缓存输入"，那是费用口径）
        let burn = satAdd(t[1], t[2])
        let agents = agentNames.enumerated().filter { agg.today[$0.offset][0] > 0 }.map { $0.element }.joined(separator: ",")
        let day = dayKey(Date().timeIntervalSince1970 * 1000)
        let body: [String: Any] = ["uid": lbUid(), "name": lbName(), "day": day,
                                   "burn": burn, "i": t[1], "o": t[2], "agents": agents]
        guard let bodyData = try? JSONSerialization.data(withJSONObject: body) else { return }
        var req = URLRequest(url: URL(string: lbURL() + "/api/lb/submit")!)
        req.httpMethod = "POST"
        req.httpBody = bodyData
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        URLSession.shared.dataTask(with: req) { [weak self] _, _, _ in
            guard let self else { return }
            let getUrl = URL(string: self.lbURL() + "/api/lb/me?uid=\(self.lbUid())&days=\(self.lbDays())")!
            URLSession.shared.dataTask(with: getUrl) { data, _, _ in
                guard let data,
                      let d = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
                      // 这个接口直接返回排名本身（`rank`/`total`/`percentile`），没有
                      // `me` 这一层。以前这里要求 `d["me"]` 存在，于是排名永远进不了
                      // 这个分支，挂件上那一行一直是空的——`burn` 才是"我上榜了没有"。
                      ((d["burn"] as? NSNumber)?.intValue ?? 0) > 0 else {
                    DispatchQueue.main.async {
                        self.hudRank?.stringValue = ""
                        self.hudRank?.isHidden = true
                    }
                    return
                }
                let rank = (d["rank"] as? NSNumber)?.intValue ?? 0
                let total = (d["total"] as? NSNumber)?.intValue ?? 0
                let pct = (d["percentile"] as? NSNumber)?.doubleValue ?? 0
                let range = self.lbRangeLabel()
                DispatchQueue.main.async {
                    self.hudRank?.stringValue = self.lang == "en"
                        ? String(format: "🌍 Global #%d/%d (%@) · beats %.0f%%", rank, total, range, pct)
                        : String(format: "🌍 全球第 %d / %d 位（%@）· 胜过 %.0f%%", rank, total, range, pct)
                    self.hudRank?.isHidden = self.hudCompact
                }
            }.resume()
        }.resume()
    }

}
