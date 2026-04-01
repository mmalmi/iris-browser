import Foundation
import Tauri
import UIKit
import WebKit

struct CreateArgs: Decodable {
  let label: String
  let url: String
  let x: Double
  let y: Double
  let width: Double
  let height: Double
  let scale: Double
  let initScript: String
  let diagnosticScript: String
  let allowedOriginRule: String?
  let actualUrlRoot: String?
}

struct LabelArgs: Decodable {
  let label: String
}

struct NavigateArgs: Decodable {
  let label: String
  let url: String
}

struct BoundsArgs: Decodable {
  let label: String
  let x: Double
  let y: Double
  let width: Double
  let height: Double
  let scale: Double
}

struct HistoryArgs: Decodable {
  let label: String
  let direction: String
}

struct ShellOverlayArgs: Decodable {
  let enabled: Bool
  let x: Double
  let y: Double
  let width: Double
  let height: Double
  let scale: Double
}

private final class BrowserEntry: NSObject, WKNavigationDelegate {
  let args: CreateArgs
  let webView: WKWebView
  let plugin: MobileBrowserPlugin

  init(args: CreateArgs, plugin: MobileBrowserPlugin) {
    self.args = args
    self.plugin = plugin

    let configuration = WKWebViewConfiguration()
    configuration.websiteDataStore = .default()
    configuration.userContentController = WKUserContentController()
    configuration.userContentController.addUserScript(
      WKUserScript(
        source: args.initScript,
        injectionTime: .atDocumentStart,
        forMainFrameOnly: true
      )
    )

    self.webView = WKWebView(frame: .zero, configuration: configuration)
    super.init()
    self.webView.navigationDelegate = self
    self.webView.backgroundColor = UIColor(red: 15 / 255, green: 15 / 255, blue: 15 / 255, alpha: 1)
    self.webView.isOpaque = true
  }

  func webView(_ webView: WKWebView, decidePolicyFor navigationAction: WKNavigationAction, decisionHandler: @escaping (WKNavigationActionPolicy) -> Void) {
    guard navigationAction.targetFrame?.isMainFrame != false else {
      decisionHandler(.allow)
      return
    }

    let url = navigationAction.request.url?.absoluteString ?? ""
    if plugin.shouldInterceptNavigation(args: args, url: url) {
      plugin.emitLocation(label: args.label, url: url, source: "navigation")
      decisionHandler(.cancel)
      return
    }

    decisionHandler(.allow)
  }

  func webView(_ webView: WKWebView, didStartProvisionalNavigation navigation: WKNavigation!) {
    guard let url = webView.url?.absoluteString else { return }
    plugin.emitLocation(label: args.label, url: url, source: "navigation")
    plugin.emitPageLoad(label: args.label, url: url, event: "started")
  }

  func webView(_ webView: WKWebView, didFinish navigation: WKNavigation!) {
    guard let url = webView.url?.absoluteString else { return }
    if plugin.shouldInjectScripts(args: args, url: url) {
      plugin.injectScripts(on: webView, args: args)
    }
    plugin.emitPageLoad(label: args.label, url: url, event: "finished")
  }

  func webView(_ webView: WKWebView, didFail navigation: WKNavigation!, withError error: Error) {
    plugin.emitDiagnostic(label: args.label, url: webView.url?.absoluteString, source: "navigation-error", error: error.localizedDescription)
  }

  func webView(_ webView: WKWebView, didFailProvisionalNavigation navigation: WKNavigation!, withError error: Error) {
    plugin.emitDiagnostic(label: args.label, url: webView.url?.absoluteString ?? args.url, source: "provisional-navigation-error", error: error.localizedDescription)
  }
}

class MobileBrowserPlugin: Plugin {
  private var browsers = [String: BrowserEntry]()
  private weak var shellWebView: WKWebView?

  override func load(webview: WKWebView) {
    DispatchQueue.main.async {
      self.shellWebView = webview
    }
  }

  @objc public func create(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(CreateArgs.self)

    DispatchQueue.main.async {
      self.destroyBrowser(label: args.label)

      let entry = BrowserEntry(args: args, plugin: self)
      entry.webView.frame = self.frame(from: args)
      if let hostView = self.contentHostView() {
        hostView.addSubview(entry.webView)
        hostView.bringSubviewToFront(entry.webView)
      }
      self.browsers[args.label] = entry
      if let url = URL(string: args.url) {
        entry.webView.load(URLRequest(url: url))
      }
      invoke.resolve()
    }
  }

  @objc public func close(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(LabelArgs.self)
    DispatchQueue.main.async {
      self.destroyBrowser(label: args.label)
      invoke.resolve()
    }
  }

  @objc public func navigate(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(NavigateArgs.self)
    DispatchQueue.main.async {
      guard let entry = self.browsers[args.label], let url = URL(string: args.url) else {
        invoke.reject("Webview \(args.label) not found")
        return
      }
      entry.webView.load(URLRequest(url: url))
      invoke.resolve()
    }
  }

  @objc public func setBounds(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(BoundsArgs.self)
    DispatchQueue.main.async {
      guard let entry = self.browsers[args.label] else {
        invoke.reject("Webview \(args.label) not found")
        return
      }
      entry.webView.frame = self.frame(from: args)
      self.contentHostView()?.bringSubviewToFront(entry.webView)
      invoke.resolve()
    }
  }

  @objc public func setShellOverlay(_ invoke: Invoke) throws {
    _ = try invoke.parseArgs(ShellOverlayArgs.self)
    DispatchQueue.main.async {
      invoke.resolve()
    }
  }

  @objc public func history(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(HistoryArgs.self)
    DispatchQueue.main.async {
      guard let entry = self.browsers[args.label] else {
        invoke.reject("Webview \(args.label) not found")
        return
      }
      switch args.direction {
      case "back":
        if entry.webView.canGoBack { entry.webView.goBack() }
      case "forward":
        if entry.webView.canGoForward { entry.webView.goForward() }
      default:
        invoke.reject("Invalid history direction")
        return
      }
      invoke.resolve()
    }
  }

  @objc public func reload(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(LabelArgs.self)
    DispatchQueue.main.async {
      guard let entry = self.browsers[args.label] else {
        invoke.reject("Webview \(args.label) not found")
        return
      }
      entry.webView.reload()
      invoke.resolve()
    }
  }

  @objc public func currentUrl(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(LabelArgs.self)
    DispatchQueue.main.async {
      guard let entry = self.browsers[args.label] else {
        invoke.reject("Webview \(args.label) not found")
        return
      }
      invoke.resolve(["url": entry.webView.url?.absoluteString])
    }
  }

  func emitLocation(label: String, url: String, source: String) {
    trigger("location", data: [
      "label": label,
      "url": url,
      "source": source,
    ])
  }

  func emitPageLoad(label: String, url: String, event: String) {
    trigger("page-load", data: [
      "label": label,
      "url": url,
      "event": event,
    ])
  }

  func emitDiagnostic(label: String, url: String?, source: String, error: String) {
    trigger("diagnostic", data: [
      "label": label,
      "url": url ?? NSNull(),
      "source": source,
      "error": error,
    ])
  }

  func shouldInterceptNavigation(args: CreateArgs, url: String) -> Bool {
    if url.hasPrefix("htree://") {
      return true
    }

    if let actualUrlRoot = args.actualUrlRoot {
      return !url.hasPrefix(actualUrlRoot)
    }

    guard let allowedOriginRule = args.allowedOriginRule else {
      return false
    }

    return origin(of: url) != allowedOriginRule
  }

  func shouldInjectScripts(args: CreateArgs, url: String) -> Bool {
    if let actualUrlRoot = args.actualUrlRoot {
      return url.hasPrefix(actualUrlRoot)
    }

    guard let allowedOriginRule = args.allowedOriginRule else {
      return true
    }

    return origin(of: url) == allowedOriginRule
  }

  func injectScripts(on webView: WKWebView, args: CreateArgs) {
    webView.evaluateJavaScript(args.initScript, completionHandler: nil)
    webView.evaluateJavaScript(args.diagnosticScript, completionHandler: nil)
    DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
      webView.evaluateJavaScript(args.initScript, completionHandler: nil)
      webView.evaluateJavaScript(args.diagnosticScript, completionHandler: nil)
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + 1.0) {
      webView.evaluateJavaScript(args.initScript, completionHandler: nil)
      webView.evaluateJavaScript(args.diagnosticScript, completionHandler: nil)
    }
  }

  private func destroyBrowser(label: String) {
    guard let entry = browsers.removeValue(forKey: label) else {
      return
    }
    entry.webView.navigationDelegate = nil
    entry.webView.stopLoading()
    entry.webView.removeFromSuperview()
  }

  private func rootView() -> UIView? {
    return manager.viewController?.view
  }

  private func contentHostView() -> UIView? {
    return shellWebView?.superview ?? rootView()
  }

  private func frame(from args: CreateArgs) -> CGRect {
    CGRect(x: args.x, y: args.y, width: max(args.width, 0), height: max(args.height, 0))
  }

  private func frame(from args: BoundsArgs) -> CGRect {
    CGRect(x: args.x, y: args.y, width: max(args.width, 0), height: max(args.height, 0))
  }

  private func origin(of urlString: String) -> String? {
    guard let components = URLComponents(string: urlString), let scheme = components.scheme else {
      return nil
    }
    guard let host = components.host else {
      return scheme
    }
    if let port = components.port {
      return "\(scheme)://\(host):\(port)"
    }
    return "\(scheme)://\(host)"
  }
}

@_cdecl("init_plugin_mobile_browser")
func initPlugin() -> Plugin {
  return MobileBrowserPlugin()
}
