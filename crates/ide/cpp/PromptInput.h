#pragma once
#include <QColor>
#include <QPlainTextEdit>
#include <QString>

class QKeyEvent;

/// The box a prompt is typed into, at the foot of a transcript.
///
/// Enter sends and Shift+Enter breaks the line, which is the convention every
/// chat box uses and the opposite of what `QPlainTextEdit` does by default.
/// The widget keeps no transcript state: it emits the text and empties itself,
/// and whether the agent is busy is told to it from outside.
class PromptInput : public QPlainTextEdit {
    Q_OBJECT
public:
    explicit PromptInput(QWidget* parent = nullptr);

    /// Locks the box while the agent is working and says so in the placeholder.
    /// An empty `message` uses the default wording; the transcript passes its
    /// own while it is replaying history.
    void setBusy(bool busy, const QString& message = QString());
    bool isBusy() const;

public Q_SLOTS:
    /// Sends whatever is typed, exactly as Enter does. A blank box, or a busy
    /// one, sends nothing.
    ///
    /// This is a slot rather than a private helper so a test can drive the box
    /// through the same path a keystroke takes, without synthesising one.
    void submit();

Q_SIGNALS:
    void submitted(const QString& text);

protected:
    void keyPressEvent(QKeyEvent* event) override;

private:
    /// Applies `m_busy` to the placeholder wording and its colour.
    void refreshPlaceholder();

    bool m_busy = false;
    /// What the placeholder says while busy, or empty for the default.
    QString m_busyMessage;
    /// The style's own placeholder colour, kept so the idle box gets it back
    /// after a busy spell has greyed it.
    QColor m_idlePlaceholderColor;
};
