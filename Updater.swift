import AppKit
import CryptoKit
import WebKit

// MARK: - 应用内自我更新（有新版本 → 图标 → 用户点一下 → 内部进程装好）
//
// 分工：**用户决定要不要更新**（菜单栏出现更新图标，点"现在更新"），
// 剩下的事由应用自己做——下载安装包 → 校验 SHA-256 → 解压 → 交给一个独立的小脚本，
// 等本进程退出后替换 /Applications 里的 .app 再重新打开。
//
// 为什么用一个脚本而不是自己替换自己：macOS 上正在运行的 .app 不能可靠地覆盖自身
// （bundle 正在被使用），而 `Process` 起的子进程在父进程退出后会交给 launchd 继续跑。
// 脚本先 `mv` 旧包成备份，复制成功再删备份；复制失败（没有写权限）就用
// `osascript … with administrator privileges` 再试一次——那是唯一会让用户看到系统
// 密码框的路径，能成功复制的正常情况不会打扰用户。
extension AppDelegate: URLSessionDownloadDelegate {
    /// 更新状态的显示面一起刷新：菜单里那一条、挂件上的蓝色标记。
    /// 菜单栏图标上的橙点由动画帧自己换成 `menuBarImage()`，不需要在这里重画。
    /// 两处显示如果各写各的，迟早会有一处忘了同步——这次就是把它们绑在一起。
    func refreshUpdateUI() {
        statusItem.menu = buildMenu()
        syncUpdateBadge()
    }

    /// 状态栏上的更新图标：在原来的跑步小人右下角加一个橙点。
    func badgedForUpdate(_ base: NSImage) -> NSImage {
        let size = base.size
        let out = NSImage(size: size)
        out.lockFocus()
        base.draw(in: NSRect(origin: .zero, size: size))
        let d: CGFloat = max(5, size.height * 0.34)
        let dot = NSRect(x: size.width - d - 0.5, y: 0.5, width: d, height: d)
        NSColor.systemOrange.setFill()
        NSBezierPath(ovalIn: dot).fill()
        NSColor.white.withAlphaComponent(0.9).setStroke()
        let ring = NSBezierPath(ovalIn: dot.insetBy(dx: 0.5, dy: 0.5))
        ring.lineWidth = 1
        ring.stroke()
        out.unlockFocus()
        return out
    }

    /// 菜单栏图标：有更新就换成带橙点的版本（动画帧也一样处理）。
    func menuBarImage(_ frame: NSImage) -> NSImage {
        updateInfo == nil ? frame : badgedForUpdate(frame)
    }

    private static var updateDir: URL {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask).first
            ?? URL(fileURLWithPath: NSTemporaryDirectory())
        let dir = base.appendingPathComponent("TokenDance", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }


    /// 把后端给的路径解析成可下载的 URL。
    ///
    /// 后端可能给两种形态：`/download/x.zip`（不带部署前缀）或
    /// `/tokendance/download/x.zip`（带前缀，`file` 字段就是这种）。所以这里只取
    /// **scheme + host** 再拼路径——用整个 telURL() 去拼会把前缀叠两次，
    /// 拿回来的是 404 的 0 字节文件（这次就是靠 sha256 校验发现的）。
    func resolve(_ path: String) -> URL? {
        if !path.hasPrefix("/") { return URL(string: path) }
        guard let base = URL(string: telURL()), let scheme = base.scheme, let host = base.host else { return nil }
        var port = ""
        if let p = base.port { port = ":" + String(p) }
        return URL(string: scheme + "://" + host + port + path)
    }

    /// 用户点了「现在更新」。
    @objc func startUpdate() {
        guard let info = updateInfo, updateTask == nil else { return }
        guard let url = resolve(info.file.isEmpty ? info.url : info.file) else { return }
        updateProgress = 0
        updatePhase = L("准备下载…", "Preparing…")
        refreshUpdateUI()
        let cfg = URLSessionConfiguration.default
        let session = URLSession(configuration: cfg, delegate: self, delegateQueue: .main)
        let task = session.downloadTask(with: url)
        updateTask = task
        updateSession = session
        task.resume()
    }

    public func urlSession(_ s: URLSession, downloadTask: URLSessionDownloadTask,
                           didWriteData bytesWritten: Int64, totalBytesWritten: Int64,
                           totalBytesExpectedToWrite: Int64) {
        guard totalBytesExpectedToWrite > 0 else { return }
        let pct = Double(totalBytesWritten) / Double(totalBytesExpectedToWrite)
        // 每 5% 重建一次菜单，够平滑也不折腾（菜单本来就只在打开时构建）
        if pct - updateProgress >= 0.05 {
            updateProgress = pct
            updatePhase = String(format: L("下载中 %.0f%%", "Downloading %.0f%%"), pct * 100)
            refreshUpdateUI()
        }
    }

    public func urlSession(_ s: URLSession, downloadTask: URLSessionDownloadTask,
                           didFinishDownloadingTo location: URL) {
        let ver = updateInfo?.version ?? "new"
        let zip = Self.updateDir.appendingPathComponent("TokenDance-\(ver)-mac.zip")
        try? FileManager.default.removeItem(at: zip)
        do {
            try FileManager.default.moveItem(at: location, to: zip)
        } catch {
            updatePhase = L("下载失败：", "Download failed: ") + error.localizedDescription
            finishUpdate(moveTo: nil)
            return
        }
        // 校验：接口给的 sha256 必须和文件对得上，否则不装
        if let want = updateSHA, !want.isEmpty {
            updatePhase = L("校验安装包…", "Verifying…")
            refreshUpdateUI()
            if let data = try? Data(contentsOf: zip, options: .mappedIfSafe) {
                let got = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
                if got != want.lowercased() {
                    updatePhase = L("安装包校验失败，已放弃", "Checksum mismatch — not installing")
                    finishUpdate(moveTo: nil)
                    return
                }
            }
        }
        installAndRestart(zip: zip, version: ver)
    }

    public func urlSession(_ s: URLSession, task: URLSessionTask, didCompleteWithError error: Error?) {
        guard let error else { return }          // 成功路径在 didFinishDownloadingTo
        updatePhase = L("下载失败：", "Download failed: ") + error.localizedDescription
        finishUpdate(moveTo: nil)
    }

    private func finishUpdate(moveTo: String?) {
        updateTask = nil
        updateSession?.finishTasksAndInvalidate()
        updateSession = nil
        if moveTo == nil { updateProgress = 0 }
        refreshUpdateUI()
    }

    /// 解压 → 写一个替换脚本 → 退出自己（脚本会等我们退出后替换并重新打开）。
    private func installAndRestart(zip: URL, version: String) {
        updatePhase = L("解压…", "Unpacking…")
        refreshUpdateUI()
        let dest = Self.updateDir.appendingPathComponent("unpack-\(version)")
        try? FileManager.default.removeItem(at: dest)
        let unzip = Process()
        unzip.executableURL = URL(fileURLWithPath: "/usr/bin/ditto")
        unzip.arguments = ["-x", "-k", zip.path, dest.path]
        do { try unzip.run(); unzip.waitUntilExit() } catch {
            updatePhase = L("解压失败", "Unpack failed")
            finishUpdate(moveTo: nil)
            return
        }
        let newApp = dest.appendingPathComponent("TokenDance.app")
        guard FileManager.default.fileExists(atPath: newApp.path) else {
            updatePhase = L("安装包里没有 TokenDance.app", "No TokenDance.app in the archive")
            finishUpdate(moveTo: nil)
            return
        }
        let oldApp = Bundle.main.bundleURL
        let script = Self.updateDir.appendingPathComponent("apply-update.sh")
        // UPDATEDIR 只用来放下载与解压的中间产物；更新成功后连同旧版本的残留一起清掉
        // （实测每次更新会留下一个 2.3 MB 的 zip 和一个解压目录，从不清就一直涨）。
        let sh = """
        #!/bin/sh
        # 等旧进程退出 → 备份旧包 → 复制新包 → 去隔离属性 → 重新打开 → 收尾
        OLD="$1"; NEW="$2"; PID="$3"; DIR="$4"
        i=0
        while kill -0 "$PID" 2>/dev/null && [ $i -lt 300 ]; do sleep 0.3; i=$((i+1)); done
        sleep 0.4
        BACKUP="$OLD.old.$$"
        if mv "$OLD" "$BACKUP" 2>/dev/null; then
          if cp -R "$NEW" "$OLD" 2>/dev/null; then
            xattr -dr com.apple.quarantine "$OLD" 2>/dev/null
            open "$OLD"
            rm -rf "$BACKUP"
            [ -n "$DIR" ] && rm -rf "$DIR"/TokenDance-*.zip "$DIR"/unpack-*
            exit 0
          fi
          mv "$BACKUP" "$OLD" 2>/dev/null
        fi
        # 没有写权限：走一次管理员授权（唯一会弹密码框的路径）
        osascript -e "do shell script \\"rm -rf '$OLD' && cp -R '$NEW' '$OLD' && xattr -dr com.apple.quarantine '$OLD'\\" with administrator privileges" \\
          && open "$OLD" && { [ -n "$DIR" ] && rm -rf "$DIR"/TokenDance-*.zip "$DIR"/unpack-*; true; }
        """
        try? sh.write(to: script, atomically: true, encoding: .utf8)
        try? FileManager.default.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
        updatePhase = L("重启以完成更新…", "Restarting to finish…")
        refreshUpdateUI()
        let p = Process()
        p.executableURL = URL(fileURLWithPath: "/bin/sh")
        p.arguments = [script.path, oldApp.path, newApp.path,
                       String(ProcessInfo.processInfo.processIdentifier), Self.updateDir.path]
        do {
            try p.run()
        } catch {
            updatePhase = L("无法启动更新进程", "Could not start the updater")
            finishUpdate(moveTo: nil)
            return
        }
        // 给菜单最后一次重绘的机会，然后退出让脚本接手
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.6) {
            NSApp.terminate(nil)
        }
    }
}
