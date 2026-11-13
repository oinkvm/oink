import SwiftUI

extension View {
    func speakerGestures(_ action: @escaping (Bool) -> Void) -> some View {
        gesture(LongPressGesture(minimumDuration: 0.5).exclusively(before: TapGesture())
            .onEnded { result in
                switch result {
                case .first: action(true)
                case .second: action(false)
                }
            })
            .accessibilityElement(children: .ignore)
            .accessibilityHidden(false)
            .accessibilityAddTraits(.isButton)
            .accessibilityAction { action(false) }
            .accessibilityAction(named: Text("长按")) { action(true) }
    }
}
