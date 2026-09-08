#include "app.h"
#include "MainWindow.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QApplication>

std::int32_t run_app() {
    static int argc = 1;
    static char name[] = "bondsymphonic";
    static char* argv[] = { name, nullptr };
    QApplication app(argc, argv);
    QApplication::setApplicationName("BondSymphonic");
    QApplication::setOrganizationName("BondSymphonic");
    QApplication::setStyle("Fusion");

    auto* controller = new AppController(&app);
    MainWindow window(controller);
    window.show();
    controller->start();
    return app.exec();
}
