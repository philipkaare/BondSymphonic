#include "MainWindow.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include <QDockWidget>
#include <QLabel>
#include <QMenuBar>
#include <QPlainTextEdit>
#include <QSplitter>
#include <QStatusBar>
#include <QTabBar>
#include <QTabWidget>
#include <QTreeView>
#include <QVBoxLayout>
#include <QWidget>

MainWindow::MainWindow(AppController* controller, QWidget* parent)
    : QMainWindow(parent), m_controller(controller) {
    setWindowTitle("BondSymphonic");
    resize(1400, 900);
    buildMenus();
    buildCentral();
    buildDocks();
    buildStatusBar();
    QObject::connect(m_controller, &AppController::connectionStateChanged, this, &MainWindow::onConnectionStateChanged);
    QObject::connect(m_controller, &AppController::statusMessageChanged, this, &MainWindow::onConnectionStateChanged);
    QObject::connect(m_controller, &AppController::daemonVersionChanged, this, &MainWindow::onConnectionStateChanged);
    // No showMessage() here: a temporary status bar message hides every addWidget()
    // widget while it is shown, including the sandbox label the details hang off.
    QObject::connect(m_controller, &AppController::prereqWarning, this, [this](const QString& msg) {
        m_sandboxLabel->setText("sandbox: prerequisites missing");
        m_sandboxLabel->setToolTip(msg);
    });
    onConnectionStateChanged();
}

void MainWindow::buildMenus() {
    auto* file = menuBar()->addMenu("&File");
    file->addAction("E&xit", this, &QWidget::close);
    menuBar()->addMenu("&Edit");
    menuBar()->addMenu("&View");
    menuBar()->addMenu("&Workspace");
    menuBar()->addMenu("&Run");
    menuBar()->addMenu("&Help");
}

void MainWindow::buildCentral() {
    auto* central = new QWidget(this);
    auto* layout = new QVBoxLayout(central);
    layout->setContentsMargins(0, 0, 0, 0);
    layout->setSpacing(0);

    m_groupTabs = new QTabBar(central);
    m_groupTabs->setExpanding(false);
    m_groupTabs->addTab("Default");
    m_agentTabs = new QTabBar(central);
    m_agentTabs->setExpanding(false);
    m_agentTabs->setDocumentMode(true);
    layout->addWidget(m_groupTabs);
    layout->addWidget(m_agentTabs);

    m_centerSplitter = new QSplitter(Qt::Horizontal, central);
    m_editorPlaceholder = new QPlainTextEdit(m_centerSplitter);
    m_editorPlaceholder->setReadOnly(true);
    m_editorPlaceholder->setPlaceholderText("Editor");
    m_agentPlaceholder = new QPlainTextEdit(m_centerSplitter);
    m_agentPlaceholder->setReadOnly(true);
    m_agentPlaceholder->setPlaceholderText("Agent");
    m_centerSplitter->addWidget(m_editorPlaceholder);
    m_centerSplitter->addWidget(m_agentPlaceholder);
    m_centerSplitter->setStretchFactor(0, 3);
    m_centerSplitter->setStretchFactor(1, 2);
    layout->addWidget(m_centerSplitter, 1);
    setCentralWidget(central);
}

void MainWindow::buildDocks() {
    auto* explorer = new QDockWidget("Explorer", this);
    explorer->setObjectName("ExplorerDock");
    auto* tabs = new QTabWidget(explorer);
    tabs->addTab(new QTreeView(tabs), "Files");
    tabs->addTab(new QTreeView(tabs), "Changes");
    explorer->setWidget(tabs);
    addDockWidget(Qt::LeftDockWidgetArea, explorer);

    auto* bottom = new QDockWidget("Output", this);
    bottom->setObjectName("BottomDock");
    auto* bottomTabs = new QTabWidget(bottom);
    bottomTabs->addTab(new QPlainTextEdit(bottomTabs), "Run");
    bottomTabs->addTab(new QPlainTextEdit(bottomTabs), "Terminal");
    bottom->setWidget(bottomTabs);
    addDockWidget(Qt::BottomDockWidgetArea, bottom);
}

void MainWindow::buildStatusBar() {
    m_daemonLabel = new QLabel(this);
    m_sandboxLabel = new QLabel("sandbox: -", this);
    m_branchLabel = new QLabel("branch: -", this);
    m_costLabel = new QLabel("$0.00", this);
    statusBar()->addWidget(m_daemonLabel);
    statusBar()->addWidget(m_sandboxLabel);
    statusBar()->addWidget(m_branchLabel);
    statusBar()->addPermanentWidget(m_costLabel);
}

void MainWindow::onConnectionStateChanged() {
    // AppController composes the full text, including the version suffix.
    m_daemonLabel->setText(m_controller->getStatusMessage());
}
