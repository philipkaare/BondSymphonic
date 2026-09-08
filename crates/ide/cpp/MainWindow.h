#pragma once
#include <QMainWindow>
class AppController;
class QLabel;
class QTabBar;
class QSplitter;
class QPlainTextEdit;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    explicit MainWindow(AppController* controller, QWidget* parent = nullptr);

private:
    void buildMenus();
    void buildCentral();
    void buildDocks();
    void buildStatusBar();
    void onConnectionStateChanged();

    AppController* m_controller;
    QTabBar* m_groupTabs = nullptr;
    QTabBar* m_agentTabs = nullptr;
    QSplitter* m_centerSplitter = nullptr;
    QPlainTextEdit* m_editorPlaceholder = nullptr;
    QPlainTextEdit* m_agentPlaceholder = nullptr;
    QLabel* m_daemonLabel = nullptr;
    QLabel* m_sandboxLabel = nullptr;
    QLabel* m_branchLabel = nullptr;
    QLabel* m_costLabel = nullptr;
};
