slint::slint! {
    export component Main inherits Window {
        title: "Slint Hello";
        width: 400px;
        height: 300px;
        background: #ff0000;
        Text {
            text: "HELLO-SLINT";
            color: #ffffff;
            font-size: 42px;
        }
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = Main::new()?;
    ui.run()
}
