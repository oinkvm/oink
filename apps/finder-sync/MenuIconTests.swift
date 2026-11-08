import AppKit

@main struct MenuIconTests {
    static func main() {
        let image = NSImage(size: NSSize(width: 16, height: 16))
        let item = OinkFinderSync.makePasteAsFileMenuItem(title: "Paste as File", image: image)
        precondition(item.title == "Paste as File")
        precondition(item.image === image, "Paste as File menu item must carry the Oink mark")
        print("Paste as File menu item carries an image")
    }
}
