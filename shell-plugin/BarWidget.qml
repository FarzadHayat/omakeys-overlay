pragma ComponentBehavior: Bound
import QtQuick
import QtQuick.Effects
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "omakeys"

  readonly property Item button: buttonItem
  readonly property bool opened: panelLoader.item ? panelLoader.item.opened === true : false
  readonly property bool popoutSwitchClosing: panelLoader.item ? panelLoader.item.popoutSwitchClosing === true : false

  implicitWidth: buttonItem.implicitWidth
  implicitHeight: barSize

  function injectPanel() {
    var target = panelLoader.item
    if (!target) return
    if ("hostWidget" in target) target.hostWidget = root
    if ("anchorItem" in target) target.anchorItem = buttonItem
    if ("bar" in target) target.bar = root.bar
    if ("settings" in target) target.settings = root.settings
  }

  function togglePanel() {
    if (panelLoader.item && panelLoader.item.toggle) panelLoader.item.toggle()
  }
  function open() {
    if (panelLoader.item && panelLoader.item.open) panelLoader.item.open()
  }
  function close() {
    if (panelLoader.item && panelLoader.item.close) panelLoader.item.close()
  }
  function closeForPopoutSwitch() {
    if (panelLoader.item && panelLoader.item.closeForPopoutSwitch)
      panelLoader.item.closeForPopoutSwitch()
  }

  onBarChanged: injectPanel()
  onSettingsChanged: injectPanel()

  BarIconButton {
    id: buttonItem
    anchors.fill: parent
    bar: root.bar
    text: ""
    tooltipText: "Omakeys"
    iconComponent: glyphComponent
    onPressed: function (b) { root.togglePanel() }
  }

  Component {
    id: glyphComponent
    Item {
      anchors.fill: parent
      Image {
        id: glyph
        anchors.centerIn: parent
        width: parent.width * 0.72
        height: width
        source: Qt.resolvedUrl("icon.svg")
        sourceSize.width: Math.round(width * Screen.devicePixelRatio)
        sourceSize.height: Math.round(height * Screen.devicePixelRatio)
        visible: false
        layer.enabled: true
      }
      MultiEffect {
        anchors.fill: glyph
        source: glyph
        colorization: 1.0
        colorizationColor: root.bar ? root.bar.foreground : Color.foreground
      }
    }
  }

  IpcHandler {
    target: "omakeys"
    function open(): void { root.open() }
    function close(): void { root.close() }
    function show(): void { root.open() }
    function hide(): void { root.close() }
    function toggle(): void { root.togglePanel() }
  }

  Loader {
    id: panelLoader
    active: true
    source: Qt.resolvedUrl("Panel.qml")
    visible: false
    onLoaded: {
      root.injectPanel()
      Qt.callLater(root.injectPanel)
    }
  }
}
