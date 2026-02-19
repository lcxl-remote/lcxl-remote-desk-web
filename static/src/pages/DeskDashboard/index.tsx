import { PageContainer } from "@ant-design/pro-components";
import { history, Outlet, useIntl, useLocation, useParams } from "@umijs/max";
import { Button, Space, Tabs } from "antd";
import { useEffect, useState } from "react";
import { ArrowLeftOutlined } from "@ant-design/icons";

const DeskDashboard: React.FC = () => {
    const { deskId } = useParams();
    const intl = useIntl();
    const location = useLocation();
    const [activeTab, setActiveTab] = useState<string>('files');

    useEffect(() => {
        if (location.pathname.includes('/filelist')) {
            setActiveTab('files');
        } else if (location.pathname.includes('/terminal')) {
            setActiveTab('terminal');
        } else if (location.pathname.includes('/desktop')) {
            setActiveTab('desktop');
        }
    }, [location.pathname]);

    const handleTabChange = (key: string) => {
        switch (key) {
            case 'files':
                history.push(`/desk/${deskId}/filelist`);
                break;
            case 'terminal':
                history.push(`/desk/${deskId}/terminal`);
                break;
            case 'desktop':
                history.push(`/desk/${deskId}/desktop`);
                break;
        }
    };

    return (
        <PageContainer
            title={
                <Space>
                    <Button icon={<ArrowLeftOutlined />} onClick={() => history.push('/desk')}>
                        {intl.formatMessage({ id: 'pages.deskDashboard.backToList', defaultMessage: 'Back to List' })}
                    </Button>
                    <span>{intl.formatMessage({ id: 'pages.deskDashboard.deskManagement', defaultMessage: 'Desk Management: {deskId}' }, { deskId })}</span>
                </Space>
            }
        >
            <Tabs
                activeKey={activeTab}
                onChange={handleTabChange}
                items={[
                    {
                        label: intl.formatMessage({ id: 'pages.deskDashboard.fileManagement', defaultMessage: 'File Management' }),
                        key: 'files',
                    },
                    {
                        label: intl.formatMessage({ id: 'pages.deskDashboard.terminal', defaultMessage: 'Terminal' }),
                        key: 'terminal',
                    },
                    {
                        label: intl.formatMessage({ id: 'pages.deskDashboard.remoteDesktop', defaultMessage: 'Remote Desktop' }),
                        key: 'desktop',
                    },
                ]}
            />
            <Outlet />
        </PageContainer>
    );
};

export default DeskDashboard;
