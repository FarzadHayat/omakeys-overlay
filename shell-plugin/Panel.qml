pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Controls
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Commons as Commons
import qs.Ui
import "Config.js" as Config

Panel {
  id: root
  moduleName: "omakeys"
  ipcTarget: "omakeys"
  manageIpc: false

  property var hostWidget: null
  property var anchorItem: null
  readonly property var barIdentity: hostWidget || root

  property bool daemonUp: false
  property bool overlayHidden: false
  property bool loadingConfig: false
  property string statusText: ""
  property string errorText: ""
  property var cfg: Config.defaultConfig()

  readonly property string badgeText: !daemonUp ? "Offline" : (overlayHidden ? "Hidden" : "Live")
  readonly property color badgeColor: !daemonUp || overlayHidden ? root.dim : Commons.Color.accent
  readonly property bool badgeHot: daemonUp && !overlayHidden

  readonly property color foreground: bar ? bar.foreground : Commons.Color.foreground
  readonly property color dim: Qt.darker(foreground, 1.55)
  readonly property string fontFamily: bar ? bar.fontFamily : Style.font.family
  // install-desktop.sh pins this to the installed binary's absolute path.
  readonly property string omakeysBin: "omakeys"

  readonly property var modeOptions: [
    { value: "always", label: "Always" },
    { value: "auto", label: "On activity" }
  ]
  readonly property var anchorOptions: [
    { value: "top_left", label: "Top left" },
    { value: "top", label: "Top" },
    { value: "top_right", label: "Top right" },
    { value: "left", label: "Left" },
    { value: "right", label: "Right" },
    { value: "bottom_left", label: "Bottom left" },
    { value: "bottom", label: "Bottom" },
    { value: "bottom_right", label: "Bottom right" }
  ]
  readonly property var labelOptions: [
    { value: "icons", label: "Icons" },
    { value: "text", label: "Text" }
  ]
  // Filled by `omakeys boards`; fallback until that returns.
  property var boardOptions: [
    { value: "auto", label: "Auto (follow typing)" },
    { value: "generic", label: "Generic" },
    { value: "loremipsum36", label: "Omakeys" },
    { value: "samsung-trio-500", label: "Samsung Trio 500" }
  ]

  function open() {
    root.controller.show()
    root.refresh()
  }

  function close() {
    root.controller.hide()
  }

  function toggle() {
    root.opened ? close() : open()
  }

  function closeForPopoutSwitch() {
    root.popoutSwitchClosing = true
    root.close()
    Qt.callLater(function () { root.popoutSwitchClosing = false })
  }

  function switchPanel(direction) {
    if (root.bar && typeof root.bar.switchPanelFrom === "function")
      return root.bar.switchPanelFrom(root.barIdentity, direction)
    return false
  }

  function refresh() {
    errorText = ""
    boardsProc.running = false
    boardsProc.running = true
    pingProc.running = false
    pingProc.running = true
  }

  function loadConfig() {
    loadingConfig = true
    configPrintProc.running = false
    configPrintProc.running = true
  }

  function patchCfg(key, value) {
    var next = Object.assign({}, root.cfg)
    next[key] = value
    root.cfg = next
    root.scheduleSave()
  }

  function scheduleSave() {
    if (root.loadingConfig || !root.daemonUp) return
    saveTimer.restart()
  }

  function saveConfig() {
    if (!daemonUp) {
      errorText = "Omakeys not running"
      return
    }
    var payload = Config.toApplyJson(cfg)
    applyProc.command = [root.omakeysBin, "config-apply", payload]
    applyProc.running = false
    applyProc.running = true
  }

  function toggleOverlay() {
    if (!daemonUp) {
      errorText = "Omakeys not running"
      return
    }
    actionProc.command = [root.omakeysBin, "toggle"]
    actionProc.running = false
    actionProc.running = true
  }

  function quitDaemon() {
    actionProc.command = [root.omakeysBin, "quit"]
    actionProc.running = false
    actionProc.running = true
  }

  function startDaemon() {
    errorText = ""
    statusText = "Starting…"
    startProc.running = false
    startProc.running = true
  }

  onOpenedChanged: if (opened) {
    refresh()
    Qt.callLater(function () { keyCatcher.forceActiveFocus() })
  }

  Timer {
    id: saveTimer
    interval: 250
    repeat: false
    onTriggered: root.saveConfig()
  }

  Timer {
    id: startPollTimer
    interval: 400
    repeat: true
    running: false
    property int tries: 0
    onTriggered: {
      tries += 1
      pingProc.running = false
      pingProc.running = true
      if (root.daemonUp || tries >= 15) {
        stop()
        tries = 0
        if (!root.daemonUp) {
          root.statusText = "Omakeys not running"
          root.errorText = "Start failed"
        }
      }
    }
  }

  Process {
    id: startProc
    // Detach so the long-lived daemon does not block this Process.
    command: ["bash", "-c", root.omakeysBin + " run >/tmp/omakeys.log 2>&1 &"]
    running: false
    onExited: function (code) {
      if (code !== 0) {
        root.errorText = "Start failed"
        root.statusText = "Omakeys not running"
        return
      }
      startPollTimer.tries = 0
      startPollTimer.restart()
    }
  }

  Process {
    id: pingProc
    command: [root.omakeysBin, "ping"]
    running: false
    stdout: StdioCollector {
      id: pingOut
      waitForEnd: true
    }
    onExited: function (code) {
      root.daemonUp = (code === 0)
      if (!root.daemonUp) {
        root.overlayHidden = false
        if (!startPollTimer.running)
          root.statusText = "Omakeys not running"
        return
      }
      var t = (pingOut.text || "").trim()
      root.overlayHidden = t.indexOf("hidden") !== -1
      root.statusText = root.overlayHidden ? "Overlay hidden" : "Daemon running"
      root.errorText = ""
      root.loadConfig()
      startPollTimer.stop()
      startPollTimer.tries = 0
    }
  }

  Process {
    id: boardsProc
    command: [root.omakeysBin, "boards"]
    running: false
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        try {
          var rows = JSON.parse(text)
          if (Array.isArray(rows) && rows.length > 0)
            root.boardOptions = rows
        } catch (e) { /* keep fallback */ }
      }
    }
  }

  Process {
    id: configPrintProc
    command: [root.omakeysBin, "config-print"]
    running: false
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: {
        root.loadingConfig = true
        root.cfg = Config.mergeParsed(text)
        Qt.callLater(function () { root.loadingConfig = false })
      }
    }
    onExited: function (code) {
      root.loadingConfig = false
      if (code !== 0) root.errorText = "Failed to read config"
    }
  }

  Process {
    id: applyProc
    running: false
    onExited: function (code) {
      if (code === 0) {
        root.errorText = ""
        if (root.daemonUp)
          root.statusText = root.overlayHidden ? "Overlay hidden" : "Daemon running"
      } else {
        root.errorText = "Save failed"
      }
    }
  }

  Process {
    id: actionProc
    running: false
    onExited: function (code) {
      if (code !== 0) {
        root.errorText = "Command failed"
        return
      }
      root.errorText = ""
      root.refresh()
    }
  }

  KeyboardPanel {
    id: panel
    anchorItem: root.anchorItem
    owner: root.barIdentity
    bar: root.bar
    open: root.opened
    focusTarget: keyCatcher
    contentWidth: panel.fittedContentWidth(Style.space(360))
    contentHeight: panel.fittedContentHeight(column.implicitHeight)

    PanelKeyCatcher {
      id: keyCatcher
      anchors.fill: parent
      onCloseRequested: root.close()
      onTabRequested: function (direction) { root.switchPanel(direction) }

      Flickable {
        anchors.fill: parent
        contentWidth: width
        contentHeight: column.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds
        interactive: contentHeight > height
        ScrollBar.vertical: ScrollBar { policy: ScrollBar.AsNeeded }

        Column {
          id: column
          width: parent.width
          spacing: Style.space(12)

          PanelHero {
            width: parent.width
            title: "Omakeys"
            meta: root.statusText
            foreground: root.foreground
            fontFamily: root.fontFamily
            trailingControl: Component {
              BorderSurface {
                implicitWidth: liveLabel.implicitWidth + Style.space(12)
                implicitHeight: liveLabel.implicitHeight + Style.space(6)
                radius: Style.cornerRadius
                color: root.badgeHot
                  ? Style.selectedFillFor(root.foreground, Commons.Color.accent)
                  : "transparent"
                borderSpec: Border.controlSpec(
                  root.badgeHot ? "selected" : "normal",
                  root.foreground,
                  root.badgeColor
                )

                Text {
                  id: liveLabel
                  anchors.centerIn: parent
                  textFormat: Text.PlainText
                  text: root.badgeText
                  color: root.badgeColor
                  font.family: root.fontFamily
                  font.pixelSize: Style.font.body
                  font.bold: true
                }
              }
            }
          }

          Text {
            visible: root.errorText !== ""
            width: parent.width
            textFormat: Text.PlainText
            text: root.errorText
            color: root.bar ? root.bar.urgent : Commons.Color.urgent
            font.family: root.fontFamily
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WordWrap
          }

          Row {
            width: parent.width
            spacing: Style.space(8)
            visible: root.daemonUp

            Button {
              width: parent.width - chordHint.width - Style.space(8)
              text: root.overlayHidden ? "Show overlay" : "Hide overlay"
              foreground: root.foreground
              onClicked: root.toggleOverlay()
            }

            Text {
              id: chordHint
              anchors.verticalCenter: parent.verticalCenter
              textFormat: Text.PlainText
              text: root.cfg.toggle_chord || ""
              color: root.dim
              font.family: root.fontFamily
              font.pixelSize: Style.font.caption
            }
          }

          Button {
            width: parent.width
            text: "Start daemon"
            foreground: root.foreground
            visible: !root.daemonUp
            onClicked: root.startDaemon()
          }

          Column {
            width: parent.width
            spacing: Style.space(12)
            visible: root.daemonUp

            ToggleDropdown {
              width: parent.width
              label: "Board"
              value: root.cfg.board || "auto"
              options: root.boardOptions
              foreground: root.foreground
              fontFamily: root.fontFamily
              onChanged: function (v) { root.patchCfg("board", v) }
            }

            ToggleDropdown {
              width: parent.width
              label: "Show when"
              value: root.cfg.mode === "hidden" ? "auto" : root.cfg.mode
              options: root.modeOptions
              foreground: root.foreground
              fontFamily: root.fontFamily
              onChanged: function (v) { root.patchCfg("mode", v) }
            }

            Column {
              width: parent.width
              spacing: Style.spacing.md
              visible: root.cfg.mode === "auto"
              Text {
                textFormat: Text.PlainText
                text: "Auto-hide timeout  " + Math.round(timeoutSlider.liveValue) + " s"
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
              PanelSlider {
                id: timeoutSlider
                width: parent.width
                bar: root.bar
                value: Math.min(10, Math.max(1, Math.round(root.cfg.timeout_ms / 1000)))
                minimum: 1
                maximum: 10
                step: 1
                integer: true
                onReleased: function (v) {
                  root.patchCfg("timeout_ms", Math.round(v) * 1000)
                }
              }
            }

            ToggleDropdown {
              width: parent.width
              label: "Position"
              value: root.cfg.anchor
              options: root.anchorOptions
              foreground: root.foreground
              fontFamily: root.fontFamily
              onChanged: function (v) { root.patchCfg("anchor", v) }
            }

            Column {
              width: parent.width
              spacing: Style.spacing.md
              Text {
                textFormat: Text.PlainText
                // Index 0..16 → 20%..100% in 5% steps (integer snap like timeout/size).
                text: "Opacity  " + (20 + Math.round(opacitySlider.liveValue) * 5) + "%"
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
              PanelSlider {
                id: opacitySlider
                width: parent.width
                bar: root.bar
                value: Math.min(16, Math.max(0, Math.round((((Number(root.cfg.opacity) || 0.8) * 100) - 20) / 5)))
                minimum: 0
                maximum: 16
                step: 1
                integer: true
                onReleased: function (v) {
                  root.patchCfg("opacity", (20 + Math.round(v) * 5) / 100)
                }
              }
            }

            Column {
              width: parent.width
              spacing: Style.spacing.md
              Text {
                textFormat: Text.PlainText
                // Index 0..8 → 20%..100% of the screen's shorter side, 10% steps.
                text: "Size  " + (20 + Math.round(sizeSlider.liveValue) * 10) + "%"
                color: root.dim
                font.family: root.fontFamily
                font.pixelSize: Style.font.bodySmall
              }
              PanelSlider {
                id: sizeSlider
                width: parent.width
                bar: root.bar
                value: Math.min(8, Math.max(0, Math.round(((Number(root.cfg.scale) || 0.50) * 100 - 20) / 10)))
                minimum: 0
                maximum: 8
                step: 1
                integer: true
                onReleased: function (v) {
                  root.patchCfg("scale", (20 + Math.round(v) * 10) / 100)
                }
              }
            }

            ToggleDropdown {
              width: parent.width
              label: "Key labels"
              value: root.cfg.label_style
              options: root.labelOptions
              foreground: root.foreground
              fontFamily: root.fontFamily
              onChanged: function (v) { root.patchCfg("label_style", v) }
            }

            ToggleDropdown {
              width: parent.width
              label: "Pointer over overlay"
              value: {
                var m = (root.cfg.hover && root.cfg.hover.mode) ? root.cfg.hover.mode : "type"
                return m === "type" ? "type" : "dodge"
              }
              options: [
                { value: "dodge", label: "Move away" },
                { value: "type", label: "Type + tooltips" }
              ]
              foreground: root.foreground
              fontFamily: root.fontFamily
              onChanged: function (v) { root.patchCfg("hover", { mode: v }) }
            }

            Button {
              width: parent.width
              text: "Stop daemon"
              foreground: root.foreground
              onClicked: root.quitDaemon()
            }
          }
        }
      }
    }
  }
}
