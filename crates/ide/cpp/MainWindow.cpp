#include "MainWindow.h"
#include "GroupBar.h"
#include "NewAgentDialog.h"
#include "bondsymphonic-ide/src/qobjects/app_controller.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/file_tree.cxxqt.h"
#include "bondsymphonic-ide/src/qobjects/group_model.cxxqt.h"
#include <QCheckBox>
#include <QDockWidget>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QMenuBar>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QSplitter>
#include <QStatusBar>
#include <QTabWidget>
#include <QTreeView>
#include <QVBoxLayout>
#include <QWidget>

MainWindow::MainWindow(AppController* controller, GroupModel* groupModel, FileTreeModel* fileTreeModel,
                       QWidget* parent)
    : QMainWindow(parent), m_controller(controller), m_groupModel(groupModel), m_fileTreeModel(fileTreeModel) {
    setWindowTitle("BondSymphonic");
    resize(1400, 900);
    buildMenus();
    buildCentral();
    buildDocks();
    buildStatusBar();
    connectController();
    onConnectionStateChanged();
    updateWorkspaceStatus();
}

void MainWindow::buildMenus() {
    auto* file = menuBar()->addMenu("&File");
    file->addAction("&New Agent…", this, &MainWindow::onNewAgent);
    file->addSeparator();
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

    m_groupBar = new GroupBar(m_groupModel, central);
    layout->addWidget(m_groupBar);

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

    QObject::connect(m_groupBar, &GroupBar::newAgentRequested, this, &MainWindow::onNewAgent);
    QObject::connect(m_groupBar, &GroupBar::destroyRequested, this, &MainWindow::onDestroyRequested);
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
    m_sandboxIdleText = "sandbox: -";
    m_daemonLabel = new QLabel(this);
    m_sandboxLabel = new QLabel(m_sandboxIdleText, this);
    m_branchLabel = new QLabel("branch: -", this);
    m_costLabel = new QLabel("$0.00", this);
    statusBar()->addWidget(m_daemonLabel);
    statusBar()->addWidget(m_sandboxLabel);
    statusBar()->addWidget(m_branchLabel);
    statusBar()->addPermanentWidget(m_costLabel);
}

void MainWindow::connectController() {
    QObject::connect(m_controller, &AppController::connectionStateChanged, this, &MainWindow::onConnectionStateChanged);
    QObject::connect(m_controller, &AppController::statusMessageChanged, this, &MainWindow::onConnectionStateChanged);
    QObject::connect(m_controller, &AppController::daemonVersionChanged, this, &MainWindow::onConnectionStateChanged);
    // No showMessage() here: a temporary status bar message hides every addWidget()
    // widget while it is shown, including the sandbox label the details hang off.
    QObject::connect(m_controller, &AppController::prereqWarning, this, [this](const QString& msg) {
        m_sandboxIdleText = "sandbox: prerequisites missing";
        m_sandboxLabel->setToolTip(msg);
        updateWorkspaceStatus();
    });

    QObject::connect(m_controller, &AppController::workspacesListed, this,
                     [this](const QString& json) { m_groupModel->reconcile(json); });
    QObject::connect(m_controller, &AppController::workspaceCreated, this,
                     [this](const QString& info, const QString& group, const QString& adapter, const QString& command) {
                         m_groupModel->addTab(info, group, adapter, command);
                     });
    QObject::connect(m_controller, &AppController::workspaceChanged, this,
                     [this](const QString& info) { m_groupModel->applyWorkspaceInfo(info); });
    QObject::connect(m_controller, &AppController::workspaceDestroyed, this,
                     [this](const QString& id) { m_groupModel->removeWorkspace(id); });
    QObject::connect(m_controller, &AppController::operationFailed, this, &MainWindow::onOperationFailed);
    QObject::connect(m_groupModel, &GroupModel::changed, this, &MainWindow::updateWorkspaceStatus);
}

void MainWindow::onConnectionStateChanged() {
    // AppController composes the full text, including the version suffix.
    m_daemonLabel->setText(m_controller->getStatusMessage());
}

void MainWindow::onNewAgent() {
    NewAgentDialog dialog(m_controller, m_groupModel, this);
    dialog.setGroup(m_groupBar->currentGroupName());
    m_newAgentDialog = &dialog;
    const int result = dialog.exec();
    m_newAgentDialog = nullptr;
    if (result != QDialog::Accepted) {
        return;
    }
    m_controller->createWorkspace(dialog.repoPath(), dialog.baseBranch(), dialog.name(), dialog.group(),
                                  dialog.adapter(), dialog.command());
}

void MainWindow::onDestroyRequested(const QString& workspaceId) {
    QMessageBox box(this);
    box.setIcon(QMessageBox::Question);
    box.setWindowTitle("Destroy workspace");
    box.setText("Destroy this workspace? Its sandbox and worktree are removed.");
    box.setStandardButtons(QMessageBox::Yes | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    auto* force = new QCheckBox("Force (discard changes)", &box);
    box.setCheckBox(force);
    if (box.exec() != QMessageBox::Yes) {
        return;
    }
    m_controller->destroyWorkspace(workspaceId, force->isChecked());
}

void MainWindow::onOperationFailed(const QString& op, const QString& message) {
    // The New Agent dialog reports its own inspection failures inline, and while
    // it is up it is modal, so a box parented to this window could not be closed.
    if (!m_newAgentDialog.isNull()) {
        if (op == "repo.inspect") {
            return;
        }
        QMessageBox::warning(m_newAgentDialog, op, message);
        return;
    }
    QMessageBox::warning(this, op, message);
}

void MainWindow::updateWorkspaceStatus() {
    const int group = m_groupModel->activeGroupIndex();
    const int tab = m_groupModel->activeTabIndex();
    const QString tabJson = m_groupModel->activeTabJson();
    if (tabJson.isEmpty()) {
        m_branchLabel->setText("branch: -");
        m_sandboxLabel->setText(m_sandboxIdleText);
        return;
    }
    const QJsonObject active = QJsonDocument::fromJson(tabJson.toUtf8()).object();
    m_branchLabel->setText(QString("branch: %1").arg(active.value("branch").toString()));
    // The word itself comes from the model; this only frames it.
    m_sandboxLabel->setText(QString("sandbox: %1").arg(m_groupModel->statusWord(group, tab)));
}
