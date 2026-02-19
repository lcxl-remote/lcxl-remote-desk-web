import { listSessions } from "@/services/desk/listSessions";
import { PageContainer, ProList } from "@ant-design/pro-components";
import { history, useIntl, useModel } from "@umijs/max";
import { Avatar, Button, Card, Tag } from "antd";
import { useEffect, useState } from "react";

const DeskList: React.FC = () => {
    const [sessions, setSessions] = useState<API.SystemInfo[]>([]);
    const intl = useIntl();
    const { initialState } = useModel('@@initialState');

    const refresh = async () => {
        const result = await listSessions();
        if (result && Array.isArray(result)) {
            setSessions(result);
            // If only one session, redirect automatically
            if (result.length === 1) {
                // history.push(/desk/${result[0].session_id}/filelist);
            }
        }
    };

    useEffect(() => {
        refresh();
        const interval = setInterval(refresh, 5000);
        return () => clearInterval(interval);
    }, []);

    const handleEnterDesk = (session_id: string) => {
        history.push(`/desk/${session_id}/filelist`);
    }

    return (
        <PageContainer>
            <ProList<any>
                onItem={(record: any) => {
                    return {
                        onClick: () => {
                            handleEnterDesk(record.session_id);
                        },
                        style: { cursor: 'pointer' },
                    };
                }}
                grid={{ gutter: 16, column: 3 }}
                itemLayout="vertical"
                rowKey="session_id"
                headerTitle={intl.formatMessage({ id: 'pages.deskList.onlineDeskServers', defaultMessage: 'Online Desk Servers' })}
                dataSource={sessions}
                metas={{
                    title: {
                        render: (_, entity) => {
                            return <div>{entity.version_info?.display_name || entity.session_id}</div>
                        }
                    },
                    subTitle: {
                        render: (_, entity) => {
                            return (
                                <>
                                    <Tag color="blue">{entity.version_info?.api_version}</Tag>
                                    <Tag color="green">{entity.version_info?.desk_type}</Tag>
                                </>
                            )
                        },
                    },
                    content: {
                        render: (_, entity) => {
                            return (
                                <div>
                                    <div>{intl.formatMessage({ id: 'pages.deskList.ip', defaultMessage: 'IP' })}: {entity.ip}</div>
                                    <div>{intl.formatMessage({ id: 'pages.deskList.sessionId', defaultMessage: 'Session ID' })}: {entity.session_id}</div>
                                </div>
                            );
                        },
                    },
                    actions: {
                        render: (_, entity) => [
                            <Button key="enter" type="primary" onClick={(e) => {
                                e.stopPropagation();
                                handleEnterDesk(entity.session_id);
                            }}>
                                {intl.formatMessage({ id: 'pages.deskList.enterManagement', defaultMessage: 'Enter Management' })}
                            </Button>,
                        ],
                    },
                }}
            />
        </PageContainer>
    );
};

export default DeskList;
