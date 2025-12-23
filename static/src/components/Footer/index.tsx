import { GithubOutlined } from '@ant-design/icons';
import { DefaultFooter } from '@ant-design/pro-components';
import React from 'react';

const Footer: React.FC = () => {
  return (
    <DefaultFooter
      copyright="lcxl"
      style={{
        background: 'none',
      }}
      links={[
        {
          key: 'github',
          title: <div><GithubOutlined /> https://github.com/lcxl</div>,
          href: 'https://github.com/lcxl',
          blankTarget: true,
        },
      ]}
    />
  );
};

export default Footer;
