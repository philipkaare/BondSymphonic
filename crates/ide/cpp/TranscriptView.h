#pragma once
#include <QList>
#include <QPointer>
#include <QSet>
#include <QString>
#include <QStringList>
#include <QWidget>

class PermissionBar;
class PromptInput;
class TranscriptModel;
class QJsonArray;
class QJsonObject;
class QComboBox;
class QEvent;
class QLabel;
class QPushButton;
class QScrollArea;
class QTimer;
class QVBoxLayout;

/// One Claude agent's conversation: the frames, the permission bar and the
/// prompt box.
///
/// The view paints and forwards, and decides nothing. What a frame says, which
/// tool is waiting, what the turn cost and whether a card is folded are all
/// read back out of the `TranscriptModel`; Allow, Deny, Interrupt and a typed
/// prompt go straight back into it. There is no "always allow" rule here and no
/// cost arithmetic: the model answers a tool it has been told to allow without
/// this view ever hearing about it.
///
/// Frames are built once and updated in place. `resetItems` is the only thing
/// that rebuilds, which is what makes replaying a long history one layout pass
/// rather than one per message.
///
/// A streaming answer is the one case where the model talks faster than a
/// person reads: every delta is an `itemChanged` for the same item, and
/// answering each of them with a read, a parse and a Markdown render made a
/// long answer cost time in the square of its length. Changes are collected and
/// applied on a short timer instead, and the growing text is painted as plain
/// text until the item is finished -- so the Markdown is parsed once, for the
/// answer the user is left looking at.
class TranscriptView : public QWidget {
    Q_OBJECT
public:
    /// Does not take ownership of `model`: the area owns both and tears them
    /// down together.
    explicit TranscriptView(TranscriptModel* model, QWidget* parent = nullptr);

    TranscriptModel* model() const;
    void setBackend(const QString &backend);
    QString backend() const { return m_backend; }

    /// Whether an `agent.start` for this pane is in flight. The prompt box says
    /// "starting the agent" only while this is true, and the two dropdowns are
    /// shut for as long: a second restart asked for while the first is in
    /// flight would race it, and the winner would be whichever process came up
    /// last.
    void setStarting(bool starting);

    /// Whether the workspace behind this pane cannot run -- its sandbox is
    /// down or never started. The prompt box says so instead of offering to
    /// talk to an agent, and the two dropdowns are shut: choosing a model
    /// restarts the agent, and there is no sandbox to restart it in. The
    /// banner above the pane is where the way back is.
    void setWorkspaceDown(bool down);

    /// Opens or closes the composer according to whether Claude Code is logged
    /// in, per the daemon's `claude_auth` prerequisite.
    ///
    /// While it is closed the foot of the pane is a single "Log in to Claude
    /// Code…" button instead of the prompt box. An agent runs `claude -p`, and
    /// `-p` mode cannot log in: typing `/login` into it answers "login is not
    /// available in this environment", so a composer offered here before the
    /// login exists can only waste what the user typed. The login itself
    /// happens in the setup terminal, which is a PTY and can.
    void setClaudeLoggedIn(bool loggedIn);

    /// `system.list_models` answered, so the dropdown is re-filled from
    /// `agentchoices::models()`'s new contents, keeping whatever it currently
    /// shows: a choice still on the new list stays selected by id, and a
    /// typed or no-longer-listed one stays as text.
    void refillModels();

    /// Whether the daemon has answered the prerequisite check at all, per the
    /// controller's `prereqsAnswered`. Changes only what the closed gate says.
    ///
    /// Before the first answer `setClaudeLoggedIn` is false because nothing is
    /// known yet, not because anyone is logged out, and a gate that read the
    /// one flag told a logged-in user "Claude Code is not logged in" and sent
    /// them to Settings -- where the same unanswered check drew an empty page.
    /// Until this is true the gate says it is checking and offers nothing:
    /// there is nothing to fix yet.
    void setPrereqsAnswered(bool answered);

    /// What the Claude Code CLI itself said when an agent could not
    /// authenticate, per the controller's `claudeAuthFailure`, or empty.
    /// Changes only what the closed gate says: while it is set the gate's
    /// sentence is this one, verbatim, over the same login button.
    ///
    /// A third setter rather than a flag on `setClaudeLoggedIn`, for the same
    /// reason `setPrereqsAnswered` is: the facts move at different times, and
    /// the gate going up for this reason is the one case where the paraphrase
    /// -- "not logged in" -- is what the daemon's own check just contradicted.
    /// The user should read what the CLI said, which names the fix.
    void setClaudeAuthFailure(const QString& sentence);

    /// What the pane says while the transcript is empty: that the agent is
    /// there, what it will answer as, where the work came from and where it
    /// happens. `origin` is `workspacelabel::origin`'s repository and base
    /// branch; `worktree` is the directory the agent is standing in.
    ///
    /// A frame the view writes, not a transcript item: it is not persisted, not
    /// replayed, and the first real message takes its place. The daemon has
    /// nothing to say until the agent speaks, and a pane that has been waiting
    /// for a prompt for a minute should say what is waiting.
    void setWelcome(const QString& name, const QString& origin, const QString& worktree);

    /// Whether the small grey lines are on show: the `turn 3 . $0.0042 . 1.2 s`
    /// under an answer, and the italic lines the agent's own start-up reports.
    ///
    /// Off hides both, live, in a transcript that is already built. Nothing
    /// else is touched -- an answer, a prompt and a tool card are what the pane
    /// is for -- and nothing is dropped: the items are still in the model, and
    /// turning it back on puts them back where they were.
    void setShowMeta(bool show);

    /// How many times the view has read transcript JSON out of the model and
    /// parsed it, whether one item or the whole list. Test seam: what says that
    /// a streamed answer costs a handful of reads rather than one per delta.
    int jsonReadCount() const;

#if defined(BS_WIDGET_TESTS)
    /// Test seam: the transcript the view reads, one JSON object per item,
    /// standing in for the model's. The offscreen checks have no daemon to fill
    /// a `TranscriptModel`, and the streaming check needs an item that grows.
    /// See the entries at the foot of `TranscriptView.cpp`.
    void setTestItems(const QStringList& items);
    /// The frames on show, for a check that reads one back.
    int frameCount() const;
    QWidget* frameAt(int index) const;
    /// The welcome's three lines, or empty when it is not on show.
    QString welcomeTextForTest() const;
    /// The view-level notices on show, newest last -- today the "switched to
    /// ..." lines. Separate from `frameCount`, which counts transcript items.
    QStringList noticeTextsForTest() const;
#endif

signals:
    /// A restart was asked for. The area turns this into the workspace id the
    /// window needs; this view knows only its model.
    void startAgentRequested();

    /// The user chose a different model or permission mode, and `optionsJson`
    /// is the whole `AgentStartOptions` that choice means -- this tab's own
    /// options, the session to resume, and the one field they changed.
    ///
    /// `claude -p` reads both flags once, when the process starts, so there is
    /// nothing to change in flight: the answer to this signal is a restart. It
    /// carries the options rather than the choice because the merge needs the
    /// session id, which lives on the model and nowhere the window can see.
    void optionsChanged(const QString& optionsJson);

    /// The user pressed "Log in to Claude Code…". The window answers by opening
    /// Settings on its Setup section; this view knows nothing about dialogs.
    void loginRequested();

protected:
    /// A palette change is the theme moving under the pane. The banner's red
    /// is mixed into the pane's own background, so it has to be mixed again
    /// rather than kept.
    void changeEvent(QEvent* event) override;

private:
  QString m_backend = QStringLiteral("claude");
    /// Mixes the banner's red into whatever the pane is now and installs it.
    /// Called from the constructor and from every palette change.
    void applyBannerWash();
    void rebuild();
    void onItemAppended(int index);
    void onItemChanged(int index);
    void onPermissionRequested();
    void onBusyChanged();
    void onStateChanged();
    /// Puts the tab's own options into the two dropdowns. Guarded, because a
    /// combo told what the agent already runs on must not read as the user
    /// asking for a restart.
    void applyOptionsToChoices();
    /// Emits [`optionsChanged`] for whatever the two dropdowns now say, unless
    /// this view is the one that just set them or they say what was sent last.
    void emitOptionsChanged();
    /// Puts [`m_pendingSwitch`] in the transcript as a view-level notice, and
    /// forgets it. Called when a different agent id arrives, which is the
    /// switch having actually happened rather than merely been asked for.
    void announceSwitch();
    /// The `--model` argument the model dropdown stands for: the id behind a
    /// label picked off the list, or whatever was typed instead. The one id
    /// that is empty -- "let Claude Code decide" -- is spelled `-`, which is
    /// how `restartOptionsChoosing` tells it apart from "leave this alone".
    QString chosenModelId() const;
    /// Rewrites the welcome's three lines and shows or hides it. Called from
    /// everything that could change any of the four things it says: the
    /// transcript filling or emptying, the dropdowns moving, and the workspace
    /// naming itself.
    void updateWelcome();
    /// Puts `message` in the banner, or takes it down when both the daemon's
    /// state and the last request are clean.
    void refreshBanner();
    /// Rewrites the closed gate from the two flags: "checking" with no button
    /// until the prerequisite check has answered, and the login sentence with
    /// the button once it has answered no.
    void refreshGate();

    /// Builds the frame for one item, connecting a tool card's toggle back to
    /// the model at `index`.
    QWidget* makeFrame(const QJsonObject& item, int index);
    /// Repaints the frame at `index`, or replaces it if the item is no longer
    /// the kind that frame was built for.
    void updateFrame(int index, const QJsonObject& item);
    /// Drops every frame. The stretch at the foot of the column stays.
    void clearFrames();
    /// Whether an item of this kind is one of the two the meta setting hides.
    bool isHiddenMeta(const QString& kind) const;
    /// How many frames are actually on show. Not [`m_frames`]'s length: a
    /// hidden meta item keeps its slot there so the list stays indexed by item,
    /// and holds a null.
    int shownFrames() const;
    /// Where in the column a frame for item `index` belongs, counting only the
    /// frames that exist. What puts a meta line back in its own place rather
    /// than at the foot when the setting is turned on again.
    int columnPositionFor(int index) const;
    /// Repaints every frame marked by [`onItemChanged`] since the last flush,
    /// one read per frame however many deltas landed on it.
    void flushChangedItems();
    /// The whole transcript as an array, for a rebuild. Counted; see
    /// [`jsonReadCount`].
    QJsonArray readItems() const;
    /// One item as an object, or an empty object for an index the model does
    /// not have. Counted; see [`jsonReadCount`].
    QJsonObject itemAt(int index) const;
    /// Whether the view is within one line of the foot of the scroll.
    bool atBottom() const;

    QPointer<TranscriptModel> m_model;
    QScrollArea* m_scroll = nullptr;
    /// The column of frames, with a stretch as its last entry.
    QVBoxLayout* m_frameLayout = nullptr;
    /// One entry per item in the model, so an index from the model indexes this
    /// directly. An entry is null for an item whose frame is not built, which
    /// today means a meta line while [`setShowMeta`] is off.
    QList<QWidget*> m_frames;
    /// See [`setShowMeta`]. True until the window says otherwise, so a view
    /// built without one behaves as it always did.
    bool m_showMeta = true;
    /// The three lines an empty pane shows, at the head of the frame column so
    /// the first real message appears under them and then replaces them. Never
    /// in [`m_frames`]: it is not an item, and a rebuild must not take it away.
    QWidget* m_welcome = nullptr;
    QLabel* m_welcomeReady = nullptr;
    QLabel* m_welcomeWhat = nullptr;
    QLabel* m_welcomeWhere = nullptr;
    /// What [`setWelcome`] was told. Empty until the window says, which is why
    /// a view built without one shows nothing rather than "  is ready.".
    QString m_welcomeName;
    QString m_welcomeOrigin;
    QString m_welcomeWorktree;
    QLabel* m_banner = nullptr;
    PermissionBar* m_permission = nullptr;
    /// The prompt box and its three buttons as one widget, so the login gate
    /// can replace the lot without touching any of them.
    QWidget* m_composer = nullptr;
    /// What stands in the composer's place until Claude Code is logged in.
    QWidget* m_loginGate = nullptr;
    /// The gate's sentence and its button; see [`refreshGate`].
    QLabel* m_gateText = nullptr;
    QPushButton* m_loginButton = nullptr;
    PromptInput* m_input = nullptr;
    QPushButton* m_interrupt = nullptr;
    /// Which model answers here, and what this agent asks before it acts. Both
    /// are `claude -p` flags read once at process start, so both are restarts;
    /// they sit under the message box because that is where the user is when
    /// the question occurs to them.
    QComboBox* m_modelChoice = nullptr;
    QComboBox* m_permissionChoice = nullptr;
    /// True while [`applyOptionsToChoices`] is setting the dropdowns. Without
    /// it, showing a tab what it already runs on would restart it.
    bool m_applyingOptions = false;
    /// What the dropdowns last agreed with the agent on. A choice that comes
    /// back to one of these is not a choice: an editable combo reports its
    /// text again every time the focus leaves it, and a restart nobody asked
    /// for is the most expensive possible answer to that.
    QString m_sentModel;
    QString m_sentMode;
    /// What the last switch chose, as the words it will be announced in, held
    /// from the combo moving until a different agent id proves it happened.
    /// Empty when there is nothing waiting to be said.
    QString m_pendingSwitch;
    /// The agent id the last notice was posted for, so re-reading the same id
    /// -- `onStateChanged` runs on every state change, not only on a restart --
    /// does not post a second one.
    QString m_announcedAgentId;
    /// Frames the view wrote itself: today the "switched to …" notices. Kept
    /// apart from `m_frames`, where an index is a transcript item's index and a
    /// widget belonging to no item would shift every frame after it onto the
    /// wrong message.
    QList<QWidget*> m_notices;
    /// See [`setStarting`].
    bool m_starting = false;
    /// See [`setWorkspaceDown`].
    bool m_workspaceDown = false;
    /// See [`setClaudeLoggedIn`]. True until told otherwise, so a view built
    /// without a window around it behaves as it always did.
    bool m_loggedIn = true;
    /// See [`setPrereqsAnswered`]. True until told otherwise, for the same
    /// reason.
    bool m_prereqsAnswered = true;
    /// See [`setClaudeAuthFailure`]. Empty until the CLI has said otherwise.
    QString m_authFailure;
    /// The agent id the pane had when the start was asked for. A different one
    /// arriving is the start answering, whether the pane had none before or was
    /// restarting one that exited.
    QString m_startFromAgentId;
    /// Whether the next range change should jump to the foot. Recomputed from
    /// the scroll position before every append, so a user who has scrolled up
    /// to read is left where they are.
    bool m_stickToBottom = true;
    /// The last `errorOccurred` message, cleared when the user sends again.
    QString m_requestError;
    /// The frames one or more `itemChanged` have landed on since the last
    /// flush. A streamed answer puts the same index in here hundreds of times
    /// and is read back once.
    QSet<int> m_changedItems;
    /// Applies [`m_changedItems`]. Single-shot, started by the first change of
    /// a burst: an answer that streams for a minute repaints twenty times a
    /// second instead of on every delta, and a lone change -- a tool card
    /// folded, a result landing -- costs one interval nobody can see.
    QTimer* m_coalesce = nullptr;
    /// See [`jsonReadCount`]. Mutable because reading an item is const.
    mutable int m_jsonReads = 0;
#if defined(BS_WIDGET_TESTS)
    /// See [`setTestItems`]. Empty, and never consulted, unless a check has
    /// installed one.
    QStringList m_testItems;
    bool m_testItemsSet = false;
#endif
};
