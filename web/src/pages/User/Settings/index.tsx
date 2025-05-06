import { changePassword } from "@/services/dfr/changePassword";
import { PageContainer, ProForm, ProFormText } from "@ant-design/pro-components";
import { history, useIntl, useModel } from "@umijs/max";
import { message } from "antd";
import { flushSync } from "react-dom";

const Settings: React.FC = () => {
    const { initialState, setInitialState } = useModel('@@initialState');
    const intl = useIntl();
    return (
        <PageContainer>

            <ProForm<API.PasswordParams & { confirm_password: string }>
                onValuesChange={(changeValues) => console.log(changeValues)}
                request={async () => {
                    return {
                        username: initialState?.currentUser?.name,
                        confirm_password: "",
                        new_username: "",
                        new_password: "",
                        password: "",
                    };
                }}

                onFinish={async (values) => {
                    console.log(values);
                    if (!values.new_username && !values.new_password) {
                        message.error(intl.formatMessage({ id: 'pages.account.settings.newUsernameOrNewPasswordRequired' }));
                        return;
                    }
                    if (values.new_password !== values.confirm_password) {
                        message.error(intl.formatMessage({ id: 'pages.account.settings.passwordNotMatch' }));
                        return;
                    }
                    await changePassword(values);
                    message.success(intl.formatMessage({ id: 'pages.account.settings.passwordUpdateSucceed' }));
                    // clear user info from state and redirect to index page
                    flushSync(() => {
                        setInitialState((s) => ({ ...s, currentUser: undefined }));
                    });
                    history.push("/");
                }}
            >
                <ProFormText name="username" label={intl.formatMessage({ id: 'pages.account.settings.originUsername' })} disabled={true} />
                <ProFormText
                    name="new_username"
                    label={intl.formatMessage({ id: 'pages.account.settings.newUsername' })}
                    hasFeedback
                    dependencies={['new_password']}
                    rules={[
                        ({ getFieldValue }) => ({
                            validator(_, value) {

                                if (!!getFieldValue('new_password') || !!value) {
                                    return Promise.resolve();
                                }
                                return Promise.reject(new Error(intl.formatMessage({ id: 'pages.account.settings.newUsernameOrNewPasswordRequired' })));
                            },
                        }),
                    ]}
                />
                <ProFormText.Password name="password" label={intl.formatMessage({ id: 'pages.account.settings.originPassword' })}
                    hasFeedback
                    dependencies={['new_username', 'new_password']}

                    rules={[
                        ({ getFieldValue }) => ({
                            validator(_, value) {

                                if (value) {
                                    return Promise.resolve();
                                }
                                if (getFieldValue('new_username')) {
                                    return Promise.reject(new Error(intl.formatMessage({ id: 'pages.account.settings.originUsernameRequired' })));
                                }
                                if (getFieldValue('new_password')) {
                                    return Promise.reject(new Error(intl.formatMessage({ id: 'pages.account.settings.originPasswordRequired' })));
                                }
                                return Promise.resolve();
                            },
                        }),
                    ]}
                />
                <ProFormText.Password name="new_password" label={intl.formatMessage({ id: 'pages.account.settings.newPassword' })}
                    hasFeedback
                    dependencies={['new_username']}
                    rules={[
                        ({ getFieldValue }) => ({
                            validator(_, value) {

                                if (!!getFieldValue('new_username') || !!value) {
                                    return Promise.resolve();
                                }
                                return Promise.reject(new Error(intl.formatMessage({ id: 'pages.account.settings.newUsernameOrNewPasswordRequired' })));
                            },
                        }),
                    ]}
                />
                <ProFormText.Password
                    name="confirm_password"
                    label={intl.formatMessage({ id: 'pages.account.settings.confirmPassword' })}
                    dependencies={['new_password']}
                    hasFeedback
                    rules={[
                        ({ getFieldValue }) => ({
                            validator(_, value) {
                                var new_password = getFieldValue('new_password');
                                new_password = new_password ? new_password : '';
                                if (new_password === value) {
                                    return Promise.resolve();
                                }
                                return Promise.reject(new Error(intl.formatMessage({ id: 'pages.account.settings.passwordNotMatch' })));
                            },
                        }),
                    ]} />
            </ProForm>
        </PageContainer>

    );
}

export default Settings;