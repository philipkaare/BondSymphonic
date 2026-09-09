#include "app.h"
#include "MainWindow.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QApplication>

std::int32_t run_app() {
    static int argc = 1;
    static char name[] = "bondsymphonic";
    static char* argv[] = { name, nullptr };
    QApplication app(argc, argv);
    QApplication::setApplicationName("BondSymphonic");
    QApplication::setOrganizationName("BondSymphonic");
    QApplication::setStyle("Fusion");

    // Parented to the application, so they outlive the window and are destroyed
    // once, on the way out of run_app.
    auto* controller = new AppController(&app);
    auto* groupModel = new GroupModel(&app);
    auto* fileTreeModel = new FileTreeModel(&app);
    MainWindow window(controller, groupModel, fileTreeModel);
    window.show();
    controller->start();
    return app.exec();
}
