/// What is shared, for the picker's second line: 「3 张照片」/「2 个文件」/「文字」.
enum ShareSummary {
    static func text(images: Int, files: Int, text: Bool, chinese: Bool) -> String {
        if images > 0 && files == 0 { return chinese ? "\(images) 张照片" : (images == 1 ? "1 photo" : "\(images) photos") }
        let count = images + files
        if count > 0 { return chinese ? "\(count) 个文件" : (count == 1 ? "1 file" : "\(count) files") }
        return text ? (chinese ? "文字" : "Text") : ""
    }
}
