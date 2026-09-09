#pragma once
#include <QFrame>
#include <QJsonObject>
#include <QString>

class QCheckBox;
class QLabel;

/// The strip that appears above the prompt box when a tool call is waiting for
/// an answer.
///
/// It decides nothing. "Always allow this tool for this session" is a flag
/// carried out with the answer, not a rule the bar applies: the transcript
/// model keeps the list and answers the next request for that tool itself, so
/// this bar never sees it.
class PermissionBar : public QFrame {
    Q_OBJECT
public:
    explicit PermissionBar(QWidget* parent = nullptr);

    /// `QWidget::show()` is still reachable; the overload below only adds to
    /// it. Without this the plain slot would be hidden by name lookup.
    using QFrame::show;

    /// Raises the bar for `pending`, one `pendingJson` object from
    /// `TranscriptModel`. Showing it again for the request already up only
    /// refreshes the text, so a repeated signal cannot clear a ticked box.
    void show(const QJsonObject& pending);

    /// Takes the bar down and forgets the request.
    void clear();

    /// The request the bar is showing, or empty when it is down.
    QString requestId() const;

Q_SIGNALS:
    void allowed(bool always);
    void denied();

private:
    QLabel* m_prompt = nullptr;
    QCheckBox* m_always = nullptr;
    /// The `request_id` the buttons answer, so an answer can never be sent for
    /// a request that has since been replaced.
    QString m_requestId;
};
