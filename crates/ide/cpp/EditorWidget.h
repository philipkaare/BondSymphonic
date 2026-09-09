#pragma once
#include <QPointer>
#include <QString>
#include <QWidget>

class CodeView;
class EditorDocument;
class QFrame;
class QLabel;
class RustHighlighter;

/// One open file: a notice bar, an external-change bar and a `CodeView`.
///
/// The widget holds no editor state. Every keystroke is forwarded to the
/// document as a `(position, removed, inserted)` edit and comes back as a
/// highlight range; whether the file is dirty, editable or saved is the
/// document's answer, never this widget's. The one flag it does keep,
/// `m_settingText`, exists only to tell a keystroke from the reset the widget
/// itself performs when the document reloads.
class EditorWidget : public QWidget {
    Q_OBJECT
public:
    /// Takes ownership of `doc`, so closing the pane ends its file watch.
    EditorWidget(EditorDocument* doc, QWidget* parent = nullptr);

    EditorDocument* document() const;
    CodeView* view() const;

    /// Asks the document to write itself back through the daemon. A read-only
    /// document answers with `saveFailed`, which this reports.
    ///
    /// There is no Ctrl+S here: the window owns one Save action for every pane,
    /// so the shortcut works wherever the focus is.
    void save();

private:
    /// Replaces the whole buffer from the document, keeping the caret and the
    /// scroll position. Runs on the first load and on every silent reload.
    void onLoaded();
    void onContentsChange(int position, int charsRemoved, int charsAdded);
    /// Shows whichever of the load error or the read-only reason applies, and
    /// locks the pane while there is a reason.
    void updateNotice();

    QPointer<EditorDocument> m_doc;
    CodeView* m_view = nullptr;
    RustHighlighter* m_highlighter = nullptr;
    QLabel* m_notice = nullptr;
    QFrame* m_externalBar = nullptr;
    /// Set while `onLoaded` is replacing the text, so the `contentsChange` that
    /// causes is not reported back as the user's edit.
    bool m_settingText = false;
    /// The last `loadFailed` message, cleared by the next successful load.
    QString m_loadError;
};
