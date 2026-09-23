#include "app.h"
#include "Branding.h"
#include "MainWindow.h"
#include "SplashScreen.h"
#include "Theme.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/changes_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/run_panel.cxxqt.h"
#include <QApplication>
#include <QObject>
#include <QStyleHints>

std::int32_t run_app() {
    static int argc = 1;
    static char name[] = "bondsymphonic";
    static char* argv[] = { name, nullptr };
    QApplication app(argc, argv);
    QApplication::setApplicationName("BondSymphonic");
    QApplication::setOrganizationName("BondSymphonic");
    QApplication::setApplicationVersion(branding::version());
    QApplication::setStyle("Fusion");
    app.setWindowIcon(branding::appIcon());

    // Parented to the application, so they outlive the window and are destroyed
    // once, on the way out of run_app.
    auto* controller = new AppController(&app);
    auto* groupModel = new GroupModel(&app);
    auto* fileTreeModel = new FileTreeModel(&app);
    auto* changesModel = new ChangesModel(&app);
    // One for the whole application, like the Changes model: a run keeps
    // running, and keeps printing, while the user is looking at another tab.
    auto* runModel = new RunPanelModel(&app);
    // After the controller, because the choice is a setting and the controller
    // is what reads the settings file; before the window, so nothing is ever
    // built against a palette it is about to be told to stop wearing.
    theme::apply(app, theme::choiceFromName(controller->theme()));
    // "Follow system" means following it as it changes, not as it was at
    // startup: Windows flips at dusk and the application is often older than
    // that. A window on either of the two explicit choices hears the flip and
    // ignores it, which is the whole point of having chosen.
    QObject::connect(app.styleHints(), &QStyleHints::colorSchemeChanged, &app,
                     [&app, controller](Qt::ColorScheme) {
                         const theme::Choice choice =
                             theme::choiceFromName(controller->theme());
                         if (choice == theme::Choice::System) {
                             theme::apply(app, choice);
                         }
                     });

    // Up before the window is even built, so the first thing on screen says the
    // IDE is checking rather than an empty Setup page and a "not logged in"
    // gate -- which is what one user watched for ten seconds and quit on. A
    // top-level of its own, so the window's construction and dock restore
    // below are exactly what they were. Not under `BS_SMOKE_SCRIPT`: that is
    // the IDE's automated-run switch (see `menuTest` in `MainWindow.cpp`), and
    // the offscreen end-to-end suite must not have a second top-level competing
    // with the window for activation.
    SplashScreen* splash = nullptr;
    if (qEnvironmentVariableIsEmpty("BS_SMOKE_SCRIPT")) {
        splash = new SplashScreen(controller);
        splash->show();
    }

    MainWindow window(controller, groupModel, fileTreeModel, changesModel, runModel);
    window.show();
    if (splash != nullptr) {
        // The window is shown behind the splash; when the splash goes the
        // window is what the user is looking at, and should be what has focus.
        // Deleted from the loop rather than from inside its own signal.
        QObject::connect(splash, &SplashScreen::finished, &window, [&window, splash] {
            window.raise();
            window.activateWindow();
            splash->deleteLater();
        });
        splash->raise();
    }
    controller->start();
    return app.exec();
}
